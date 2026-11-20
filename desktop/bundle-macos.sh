#!/bin/sh
# Build podbag.app. Use --with-cli to include the CLI and its Linux VM guest.
#   desktop/bundle-macos.sh [debug|release] [--with-cli]
# For --with-cli, build the guest first with dev/vm.sh run dev/macos/build-guest.sh.
# Open it with: open target/release/podbag.app
set -eu
cd "$(dirname "$0")/.."
profile=${1:-release}
with_cli=${2:-}
usage() {
  echo "usage: desktop/bundle-macos.sh [debug|release] [--with-cli]" >&2
  exit 2
}
[ "$#" -le 2 ] || usage
case $with_cli in
  '' | --with-cli) ;;
  *) usage ;;
esac
case $profile in
  release) set -- --release ;;
  debug) set -- ;;
  *) usage ;;
esac
if [ "$with_cli" = --with-cli ]; then
  cargo build "$@" --bin podbag --features embed-guest
fi
cargo build "$@" -p podbag-desktop
target=${CARGO_TARGET_DIR:-target}/$profile
app=$target/podbag.app
version=$(sed -n 's/^version = "\(.*\)"/\1/p' desktop/Cargo.toml | head -n 1)
stage=$(mktemp -d "$target/.podbag.XXXXXX")
trap 'rm -rf "$stage"' EXIT
bundle=$stage/podbag.app
mkdir -p "$bundle/Contents/MacOS" "$bundle/Contents/Resources"
cp "$target/podbag-desktop" "$bundle/Contents/MacOS/podbag-desktop"
cp desktop/icons/icon.icns "$bundle/Contents/Resources/icon.icns"
cat > "$bundle/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>podbag</string>
  <key>CFBundleDisplayName</key><string>podbag</string>
  <key>CFBundleExecutable</key><string>podbag-desktop</string>
  <key>CFBundleIdentifier</key><string>io.github.steelcrab.podbag</string>
  <key>CFBundleIconFile</key><string>icon</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>$version</string>
  <key>CFBundleVersion</key><string>$version</string>
  <key>LSMinimumSystemVersion</key><string>13.0</string>
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
PLIST
if [ "$with_cli" = --with-cli ]; then
  cp "$target/podbag" "$bundle/Contents/MacOS/podbag"
  # Sign the CLI first so signing the app preserves its virtualization entitlement.
  dev/macos/sign.sh "$bundle/Contents/MacOS/podbag"
fi
codesign --force --sign - "$bundle"
rm -rf "$app"
mv "$bundle" "$app"
echo "$app"
