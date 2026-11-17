#!/bin/sh
# The Windows build must compile (the engine itself runs in WSL2 on Windows).
# Needs: rustup target add x86_64-pc-windows-gnu; brew install mingw-w64.
set -eu
cd "$(dirname "$0")/.."
RUSTFLAGS="-D warnings" cargo check -q --target x86_64-pc-windows-gnu
echo "OK windows check"
