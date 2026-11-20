#!/bin/sh
# Regenerate src/cri/pb.rs from dev/cri/api.proto.
set -eu
repo=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-${TMPDIR:-/tmp}/podbag-cri-gen-target} \
  cargo run --quiet --manifest-path "$repo/dev/cri/gen/Cargo.toml" -- "$repo"
rustfmt --edition 2024 "$repo/src/cri/pb.rs"
echo "wrote $repo/src/cri/pb.rs"
