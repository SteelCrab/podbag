#!/bin/sh
# Give an openpod binary the virtualization entitlement (an ad-hoc signature: for this Mac).
# Usage: dev/macos/sign.sh [path/to/openpod]   (default: target/debug/openpod)
set -eu
cd "$(dirname "$0")/../.."
bin=${1:-target/debug/openpod}
codesign --force --sign - --entitlements dev/macos/openpod.entitlements "$bin"
