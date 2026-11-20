#!/usr/bin/env bash
# Prepare a GitHub Ubuntu runner for podbag's container tests: rootless containers (user
# namespaces, subordinate IDs, pasta) and composefs as root (the EROFS module).
set -euo pipefail

sudo apt-get update -qq
sudo DEBIAN_FRONTEND=noninteractive apt-get install -y -qq uidmap passt >/dev/null

# Ubuntu's AppArmor restricts unprivileged user namespaces.
sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0 >/dev/null || true

for f in /etc/subuid /etc/subgid; do
  grep -q "^$USER:" "$f" || echo "$USER:100000:65536" | sudo tee -a "$f" >/dev/null
done

# Tests set the overlay module's parameters before any container loads it.
sudo modprobe overlay 2>/dev/null || true

# Some runner kernels ship EROFS with their extra modules.
if ! sudo modprobe erofs 2>/dev/null; then
  sudo DEBIAN_FRONTEND=noninteractive apt-get install -y -qq \
    "linux-modules-extra-$(uname -r)" >/dev/null 2>&1 || true
  sudo modprobe erofs 2>/dev/null || echo "no EROFS on $(uname -r): root mode uses hard-link trees"
fi
