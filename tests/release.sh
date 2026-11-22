#!/bin/sh
# Test the release scripts in dev/ci with fake CI artifacts and a fake changelog.
set -eu

repo=$(cd "$(dirname "$0")/.." && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
export LC_ALL=C

expect_failure() { # MESSAGE COMMAND...
  message=$1
  shift
  if "$@" > "$work/log" 2>&1; then
    cat "$work/log" >&2
    exit 1
  fi
  grep -F -- "$message" "$work/log" > /dev/null || {
    cat "$work/log" >&2
    exit 1
  }
}

# The artifacts of a CI run, as actions/download-artifact writes them.
artifacts=$work/artifacts
put() { # ARTIFACT FILE
  mkdir -p "$artifacts/$1"
  echo "$1/$2" > "$artifacts/$1/$2"
}
for arch in x86_64 aarch64; do
  put "podbag-$arch-linux" podbag
  put "podbag-$arch-macos" "podbag-$arch-macos"
  put "podbag-desktop-$arch-macos" "podbag-desktop-$arch-macos.zip"
  put "podbag-$arch-windows" podbag.exe
  put "podbag-vm-guest-$arch" Image
done
put podbag-desktop-x86_64-windows "Podbag Desktop_9.8.7_x64_en-US.msi"
put podbag-desktop-aarch64-windows "Podbag Desktop_9.8.7_arm64_en-US.msi"
put podbag-desktop-x86_64-windows-screenshot podbag-desktop-x86_64.png
put podbag-aarch64-android podbag
put podbag-desktop-amd64 podbag-desktop_9.8.7-1_amd64.deb
put podbag-desktop-amd64 podbag-desktop-9.8.7-1.x86_64.rpm
put podbag-desktop-arm64 podbag-desktop_9.8.7-1_arm64.deb
put podbag-desktop-arm64 podbag-desktop-9.8.7-1.aarch64.rpm

sh "$repo/dev/ci/release-files.sh" "$artifacts" 9.8.7 "$work/out" > "$work/log"
ls "$work/out" > "$work/names"
cat > "$work/want" <<'EOF'
SHA256SUMS
podbag-aarch64-android
podbag-aarch64-linux
podbag-aarch64-macos
podbag-aarch64-windows.exe
podbag-desktop-9.8.7-1.aarch64.rpm
podbag-desktop-9.8.7-1.x86_64.rpm
podbag-desktop-aarch64-macos.zip
podbag-desktop-aarch64-windows.msi
podbag-desktop-x86_64-macos.zip
podbag-desktop-x86_64-windows.msi
podbag-desktop_9.8.7-1_amd64.deb
podbag-desktop_9.8.7-1_arm64.deb
podbag-x86_64-linux
podbag-x86_64-macos
podbag-x86_64-windows.exe
EOF
diff "$work/want" "$work/names"
grep -Fx 'podbag-desktop-aarch64-windows/Podbag Desktop_9.8.7_arm64_en-US.msi' \
  "$work/out/podbag-desktop-aarch64-windows.msi" > /dev/null
echo 'PASS  Name every release file the way installers and updates look for it'

[ "$(wc -l < "$work/out/SHA256SUMS")" -eq 15 ]
if command -v sha256sum > /dev/null 2>&1; then
  (cd "$work/out" && sha256sum -c --quiet SHA256SUMS)
else
  (cd "$work/out" && shasum -a 256 -c --quiet SHA256SUMS)
fi
echo 'PASS  List each release file in SHA256SUMS'

expect_failure "is not empty" sh "$repo/dev/ci/release-files.sh" "$artifacts" 9.8.7 "$work/out"
echo 'PASS  Refuse to mix files into a folder that is not empty'

expect_failure "podbag-desktop_9.9.9-*_amd64.deb, found 0" \
  sh "$repo/dev/ci/release-files.sh" "$artifacts" 9.9.9 "$work/out2"
echo 'PASS  Refuse Linux packages of another version'

mv "$artifacts/podbag-aarch64-android" "$work/android"
expect_failure "artifact podbag-aarch64-android: want one file like podbag, found 0" \
  sh "$repo/dev/ci/release-files.sh" "$artifacts" 9.8.7 "$work/out3"
mv "$work/android" "$artifacts/podbag-aarch64-android"
echo 'PASS  Stop when an artifact is missing'

put podbag-desktop-x86_64-windows "Podbag Desktop_9.8.6_x64_en-US.msi"
expect_failure "artifact podbag-desktop-x86_64-windows: want one file like *.msi, found 2" \
  sh "$repo/dev/ci/release-files.sh" "$artifacts" 9.8.7 "$work/out4"
echo 'PASS  Stop when an artifact has more than one matching file'

cat > "$work/CHANGELOG.md" <<'EOF'
# Changelog

## v9.8.10 (2026-12-02)

- Not this one.

## v9.8.7 (2026-12-01)

### Desktop

- One change.

- Another change.

## v9.8.6 (2026-11-30)

- An older change.
EOF
sh "$repo/dev/ci/release-notes.sh" v9.8.7 "$work/CHANGELOG.md" > "$work/notes"
printf '%s\n' '### Desktop' '' '- One change.' '' '- Another change.' > "$work/want"
diff "$work/want" "$work/notes"
echo 'PASS  Take the release notes from the changelog section of the tag'

expect_failure "has no section for v9.8" sh "$repo/dev/ci/release-notes.sh" v9.8 "$work/CHANGELOG.md"
echo 'PASS  Refuse a tag without a changelog section'

version=$(sed -n 's/^version = "\(.*\)"$/\1/p' "$repo/Cargo.toml" | head -n 1)
sh "$repo/dev/ci/release-check.sh" "v$version" > "$work/log"
echo 'PASS  Accept the tag of the current version'

expect_failure "but the tag is v99.0.0" sh "$repo/dev/ci/release-check.sh" v99.0.0
expect_failure "not a stable release tag" sh "$repo/dev/ci/release-check.sh" "v$version-rc.1"
echo 'PASS  Refuse a tag of another version or a prerelease'
