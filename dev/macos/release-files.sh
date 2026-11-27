#!/bin/sh
# Build the macOS release files for this Mac's CPU in target/release-files:
#   podbag-<arch>-macos              the CLI with its Linux VM guest
#   podbag-desktop-<arch>-macos.dmg  podbag.app with that CLI, installed by drag and drop
#   podbag-desktop-<arch>-macos.zip  the same app, for install.sh and the app's own updates
# Build the guest for the same CPU first (dev/macos/build-guest.sh).
# The files are ad-hoc signed, not notarized: see "Open it the first time" in README.md.
set -eu
cd "$(dirname "$0")/../.."
case $(uname -m) in
  arm64) arch=aarch64 ;;
  x86_64) arch=x86_64 ;;
  *) echo "no macOS release for $(uname -m)" >&2; exit 1 ;;
esac
desktop/bundle-macos.sh release --with-cli
dev/macos/sign.sh target/release/podbag
out=target/release-files
mkdir -p "$out"
cp target/release/podbag "$out/podbag-$arch-macos"
rm -f "$out/podbag-desktop-$arch-macos.zip"
ditto -c -k --keepParent target/release/podbag.app "$out/podbag-desktop-$arch-macos.zip"
dev/macos/dmg.sh target/release/podbag.app "$out/podbag-desktop-$arch-macos.dmg"
ls -l "$out"
