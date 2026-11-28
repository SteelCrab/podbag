#!/bin/sh
# Build both native packages from the workspace root on Linux.
set -eu
cd "$(dirname "$0")/../.."

case "$(uname -m)" in
    x86_64) ARCH=amd64 ;;
    aarch64) ARCH=arm64 ;;
    *) echo "Unsupported architecture: $(uname -m)" >&2; exit 1 ;;
esac
metadata=$(cargo metadata --locked --no-deps --format-version 1)
VERSION=$(printf '%s\n' "$metadata" | jq -er '.packages[] | select(.name == "podbag-desktop") | .version')
target=$(printf '%s\n' "$metadata" | jq -er '.target_directory')
BIN_DIR=$target/release
export ARCH VERSION BIN_DIR

# The tray item builds on GTK 3 (libgtk-3-dev).
cargo build --locked --release -p podbag -p podbag-desktop --features podbag-desktop/linux-tray
mkdir -p "$target/packages"
for format in deb rpm; do
    nfpm package --config desktop/linux/nfpm.yaml --packager "$format" --target "$target/packages/"
done
