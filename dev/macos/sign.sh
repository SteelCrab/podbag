#!/bin/sh
# Give a podbag binary the virtualization entitlement (an ad-hoc signature: for this Mac).
# Usage: dev/macos/sign.sh [path/to/podbag]   (default: target/debug/podbag)
set -eu
cd "$(dirname "$0")/../.."
bin=${1:-target/debug/podbag}
codesign --force --sign - --entitlements dev/macos/podbag.entitlements "$bin"
