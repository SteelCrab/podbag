#!/bin/sh
# Make a disk image that installs podbag.app by drag and drop: the window shows the app
# next to a link to /Applications.
# Usage: dev/macos/dmg.sh path/to/podbag.app path/to/out.dmg
set -eu
[ "$#" -eq 2 ] || {
  echo "usage: dev/macos/dmg.sh APP DMG" >&2
  exit 2
}
app=$1 dmg=$2
[ -d "$app" ] || {
  echo "$app is not an app bundle" >&2
  exit 1
}
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
# ditto keeps the signature and the extended attributes.
ditto "$app" "$stage/${app##*/}"
ln -s /Applications "$stage/Applications"
rm -f "$dmg"
# hdiutil sometimes fails with "Resource busy" on CI machines: try again.
tries=0
until hdiutil create -quiet -volname podbag -srcfolder "$stage" -fs HFS+ \
  -format UDZO -ov "$dmg"; do
  tries=$((tries + 1))
  [ "$tries" -lt 3 ] || exit 1
  sleep 5
done
echo "$dmg"
