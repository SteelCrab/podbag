#!/bin/sh
# Run the installer against local release files, without a network connection or sudo.
set -eu

installer=$(dirname "$0")/../install.sh
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
export PODBAG_INSTALL_DIR="$work/install dir" PODBAG_SKIP_APPARMOR=1
export fixture="$work/release"
mkdir -p "$work/bin" "$fixture"
export PATH="$work/bin:$PATH"

cat > "$work/bin/curl" <<'EOF'
#!/bin/sh
while [ "$1" != -o ]; do shift; done
case "$3" in
  */SHA256SUMS) cp "$fixture/SHA256SUMS" "$2" ;;
  *.zip) cp "$fixture/desktop.zip" "$2" ;;
  *) cp "$fixture/podbag" "$2" ;;
esac
EOF
chmod +x "$work/bin/curl"

checksum() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$fixture/podbag"
  else
    shasum -a 256 "$fixture/podbag"
  fi | awk '{print $1}'
}

write_manifest() {
  digest=$(checksum)
  for asset in podbag-aarch64-linux podbag-x86_64-linux podbag-aarch64-macos podbag-x86_64-macos; do
    printf '%s  %s\n' "$digest" "$asset"
  done > "$fixture/SHA256SUMS"
}

expect_failure() {
  if sh "$installer" > "$work/log" 2>&1; then
    cat "$work/log" >&2
    exit 1
  fi
  grep -F "$1" "$work/log" >/dev/null
  cmp "$work/original" "$PODBAG_INSTALL_DIR/podbag"
}

printf '%s\n' '#!/bin/sh' 'echo podbag-test' > "$fixture/podbag"
write_manifest
sh "$installer" > "$work/log" 2>&1
cmp "$fixture/podbag" "$PODBAG_INSTALL_DIR/podbag"
cp "$fixture/podbag" "$work/original"
echo 'PASS  Install a verified release in a custom directory'

printf '%s\n' '#!/bin/sh' 'exit 1' > "$fixture/podbag"
expect_failure 'does not match SHA256SUMS'
echo 'PASS  Preserve the installed binary on checksum mismatch'

write_manifest
expect_failure 'cannot run'
echo 'PASS  Preserve the installed binary when the new version check fails'

# On a Mac: the build for its CPU, also from a shell that Rosetta runs.
printf '%s\n' '#!/bin/sh' 'echo podbag-test' > "$fixture/podbag"
write_manifest
cat > "$work/bin/uname" <<'EOF'
#!/bin/sh
case $1 in
  -s) echo Darwin ;;
  -m) echo "$fake_cpu" ;;
esac
EOF
cat > "$work/bin/sysctl" <<'EOF'
#!/bin/sh
[ -n "$fake_translated" ] || exit 1
echo "$fake_translated"
EOF
chmod +x "$work/bin/uname" "$work/bin/sysctl"

expect_file() {
  if ! fake_cpu=$1 fake_translated=$2 sh "$installer" > "$work/log" 2>&1 ||
    ! grep -F "Downloading $3 (" "$work/log" >/dev/null; then
    cat "$work/log" >&2
    exit 1
  fi
}

expect_file arm64 0 podbag-aarch64-macos
echo 'PASS  Install the Apple silicon build on Apple silicon'
expect_file x86_64 '' podbag-x86_64-macos
echo 'PASS  Install the Intel build on an Intel Mac'
expect_file x86_64 1 podbag-aarch64-macos
echo 'PASS  Install the Apple silicon build from a shell that Rosetta runs'

# Podbag Desktop on a Mac: the app from the release's .zip, into the Applications folder.
# A fake ditto unpacks the fixture's podbag.app: the test also runs on Linux.
export PODBAG_APP_DIR="$work/Applications dir"
cat > "$work/bin/ditto" <<'EOF'
#!/bin/sh
[ "$1 $2" = "-x -k" ] || exit 1
cmp -s "$3" "$fixture/desktop.zip" || exit 1
mkdir -p "$4" && cp -R "$fixture/app/podbag.app" "$4/"
EOF
chmod +x "$work/bin/ditto"
make_app() { # VERSION-LINE
  rm -rf "$fixture/app"
  mkdir -p "$fixture/app/podbag.app/Contents/MacOS"
  printf '%s\n' '#!/bin/sh' "$1" > "$fixture/app/podbag.app/Contents/MacOS/podbag"
  chmod +x "$fixture/app/podbag.app/Contents/MacOS/podbag"
  echo "$1" > "$fixture/desktop.zip"
  for arch in aarch64 x86_64; do
    if command -v sha256sum >/dev/null 2>&1; then
      sha256sum "$fixture/desktop.zip"
    else
      shasum -a 256 "$fixture/desktop.zip"
    fi | awk -v f="podbag-desktop-$arch-macos.zip" '{ print $1 "  " f }'
  done > "$fixture/SHA256SUMS"
}
desktop() { fake_cpu=arm64 PODBAG_DESKTOP=1 sh "$installer" > "$work/log" 2>&1; }

make_app 'echo podbag 9.8.7'
if ! desktop || ! grep -F 'Downloading podbag-desktop-aarch64-macos.zip (' "$work/log" >/dev/null ||
  ! cmp "$fixture/app/podbag.app/Contents/MacOS/podbag" \
    "$PODBAG_APP_DIR/podbag.app/Contents/MacOS/podbag"; then
  cat "$work/log" >&2
  exit 1
fi
grep -F "Installed $PODBAG_APP_DIR/podbag.app (podbag 9.8.7)" "$work/log" >/dev/null
echo 'PASS  Install Podbag Desktop into the Applications folder on a Mac'

make_app 'echo podbag 9.8.8'
desktop || { cat "$work/log" >&2; exit 1; }
cmp "$fixture/app/podbag.app/Contents/MacOS/podbag" \
  "$PODBAG_APP_DIR/podbag.app/Contents/MacOS/podbag"
[ "$(ls -A "$PODBAG_APP_DIR")" = podbag.app ] || { ls -A "$PODBAG_APP_DIR" >&2; exit 1; }
echo 'PASS  Replace an installed Podbag Desktop and leave nothing else behind'

cp -R "$PODBAG_APP_DIR/podbag.app" "$work/app-before"
make_app 'exit 1'
if desktop || ! grep -F 'cannot run' "$work/log" >/dev/null; then
  cat "$work/log" >&2
  exit 1
fi
diff -r "$work/app-before" "$PODBAG_APP_DIR/podbag.app"
make_app 'echo podbag 9.9.9'
echo corrupt >> "$fixture/desktop.zip"
if desktop || ! grep -F 'does not match SHA256SUMS' "$work/log" >/dev/null; then
  cat "$work/log" >&2
  exit 1
fi
diff -r "$work/app-before" "$PODBAG_APP_DIR/podbag.app"
echo 'PASS  Keep the installed Podbag Desktop when the new one fails its checks'

cat > "$work/bin/uname" <<'EOF'
#!/bin/sh
case $1 in
  -s) echo Linux ;;
  -m) echo x86_64 ;;
esac
EOF
if PODBAG_DESKTOP=1 sh "$installer" > "$work/log" 2>&1 || ! grep -F '.deb or .rpm' "$work/log" >/dev/null; then
  cat "$work/log" >&2
  exit 1
fi
echo 'PASS  Point Linux users to the packages for Podbag Desktop'
