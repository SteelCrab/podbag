#!/bin/sh
# Build the macOS release files for this Mac's CPU in target/release-files:
#   podbag-<arch>-macos              the CLI with its Linux VM guest
#   podbag-desktop-<arch>-macos.zip  podbag.app with that CLI
# Build the guest for the same CPU first (dev/macos/build-guest.sh).
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
ls -l "$out"
