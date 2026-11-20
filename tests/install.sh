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
  for asset in podbag-aarch64-linux podbag-x86_64-linux podbag-aarch64-macos; do
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
