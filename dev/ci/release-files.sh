#!/bin/sh
# Put the release files from CI artifacts into OUT under their release names, and write
# OUT/SHA256SUMS. Stop when a file is missing.
#   dev/ci/release-files.sh ARTIFACTS VERSION OUT
# ARTIFACTS has one folder per artifact (actions/download-artifact without a name).
set -eu
[ "$#" -eq 3 ] || {
  echo "usage: dev/ci/release-files.sh ARTIFACTS VERSION OUT" >&2
  exit 2
}
src=$1 v=$2 out=$3
export LC_ALL=C
mkdir -p "$out"
[ -z "$(ls -A "$out")" ] || {
  echo "$out is not empty" >&2
  exit 1
}

# take ARTIFACT PATTERN [NAME]: copy the one file of ARTIFACT that matches PATTERN to
# OUT/NAME (default: its own name).
take() {
  n=0
  for f in "$src/$1"/$2; do
    [ -f "$f" ] || continue
    n=$((n + 1))
    file=$f
  done
  [ "$n" -eq 1 ] || {
    echo "artifact $1: want one file like $2, found $n" >&2
    exit 1
  }
  cp "$file" "$out/${3:-${file##*/}}"
}

for arch in x86_64 aarch64; do
  take "podbag-$arch-linux" podbag "podbag-$arch-linux"
  take "podbag-$arch-macos" "podbag-$arch-macos"
  take "podbag-desktop-$arch-macos" "podbag-desktop-$arch-macos.zip"
  take "podbag-$arch-windows" podbag.exe "podbag-$arch-windows.exe"
  take "podbag-desktop-$arch-windows" '*.msi' "podbag-desktop-$arch-windows.msi"
done
take podbag-aarch64-android podbag podbag-aarch64-android
take podbag-desktop-amd64 "podbag-desktop_$v-*_amd64.deb"
take podbag-desktop-amd64 "podbag-desktop-$v-*.x86_64.rpm"
take podbag-desktop-arm64 "podbag-desktop_$v-*_arm64.deb"
take podbag-desktop-arm64 "podbag-desktop-$v-*.aarch64.rpm"

cd "$out"
set -- *
if command -v sha256sum > /dev/null 2>&1; then
  sha256sum -- "$@"
else
  shasum -a 256 -- "$@"
fi > SHA256SUMS
cat SHA256SUMS
