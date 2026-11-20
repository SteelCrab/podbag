#!/bin/sh
# Build podbag.app: the desktop app as a macOS app named podbag, with its icon.
#   desktop/bundle-macos.sh [debug|release]      (default: release)
# Writes target/<profile>/podbag.app. The executable inside keeps its file name,
# podbag-desktop: `podbag` is the command-line program, in the same target folder.
# Open it with: open target/release/podbag.app
set -eu
cd "$(dirname "$0")/.."
profile=${1:-release}
case $profile in
  release) cargo build --release -p podbag-desktop ;;
  debug) cargo build -p podbag-desktop ;;
  *) echo "usage: desktop/bundle-macos.sh [debug|release]" >&2; exit 2 ;;
esac
target=${CARGO_TARGET_DIR:-target}/$profile
app=$target/podbag.app
version=$(sed -n 's/^version = "\(.*\)"/\1/p' desktop/Cargo.toml | head -n 1)
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$target/podbag-desktop" "$app/Contents/MacOS/podbag-desktop"
cp desktop/icons/icon.icns "$app/Contents/Resources/icon.icns"
cat > "$app/Contents/Info.plist" <<PLIST
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
# An ad-hoc signature, for this Mac.
codesign --force --sign - "$app" >/dev/null 2>&1 || true
echo "$app"
