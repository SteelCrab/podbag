#!/usr/bin/env bash
# Provision the openpod-dev VM. Safe to run more than once.
set -euo pipefail

repo="$(cd "$(dirname "$0")/.." && pwd)"
user="$(id -un)"

sudo apt-get update -qq
sudo DEBIAN_FRONTEND=noninteractive apt-get install -y -qq build-essential curl file uidmap passt musl-tools >/dev/null

# Rootless containers need a subordinate ID range for this user.
grep -q "^$user:" /etc/subuid 2>/dev/null || sudo usermod --add-subuids 100000-165535 "$user"
grep -q "^$user:" /etc/subgid 2>/dev/null || sudo usermod --add-subgids 100000-165535 "$user"

# Ubuntu 24.04+ restricts unprivileged user namespaces to profiled programs.
sudo install -m 0644 "$repo/dev/apparmor/openpod-dev" /etc/apparmor.d/openpod-dev
sudo apparmor_parser -r /etc/apparmor.d/openpod-dev

if ! command -v rustup >/dev/null 2>&1 && [ ! -x "$HOME/.cargo/bin/rustup" ]; then
  curl -fsSL https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain stable >/dev/null
fi
# shellcheck source=/dev/null
. "$HOME/.cargo/env"
rustup target add "$(uname -m)-unknown-linux-musl" >/dev/null

if [ ! -x "$HOME/openpod-fixtures/alpine/bin/busybox" ]; then
  "$repo/dev/fetch-rootfs.sh" "$HOME/openpod-fixtures/alpine"
fi

# Docker and Podman for the comparison in docs/benchmarks.md (dev/bench/run.sh).
if [ "${1:-}" = "--bench" ]; then
  sudo DEBIAN_FRONTEND=noninteractive apt-get install -y -qq docker.io podman >/dev/null
  # Docker runs only while it is measured.
  sudo systemctl disable docker.service docker.socket containerd.service >/dev/null 2>&1 || true
  "$repo/dev/bench/docker-off.sh"
fi

echo "provisioned openpod-dev"
