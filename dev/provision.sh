#!/usr/bin/env bash
# Provision the openpod-dev VM. Safe to run more than once.
set -euo pipefail

repo="$(cd "$(dirname "$0")/.." && pwd)"
user="$(id -un)"

sudo apt-get update -qq
sudo DEBIAN_FRONTEND=noninteractive apt-get install -y -qq build-essential curl file uidmap passt musl-tools erofs-utils >/dev/null

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

# The docker CLI for the Docker API tests (tests/api_*.rs). Ubuntu ships it only with the
# engine (docker.io); the engine stays off, except while dev/bench/run.sh measures it.
if ! command -v docker >/dev/null; then
  sudo DEBIAN_FRONTEND=noninteractive apt-get install -y -qq docker.io >/dev/null
  sudo systemctl disable docker.service docker.socket containerd.service >/dev/null 2>&1 || true
  "$repo/dev/bench/docker-off.sh"
fi

# Podman and Kiri (and Docker, above) for the comparison in docs/benchmarks.md
# (dev/bench/run.sh).
if [ "${1:-}" = "--bench" ]; then
  sudo DEBIAN_FRONTEND=noninteractive apt-get install -y -qq podman >/dev/null
  # Kiri 0.2.1, engine only (its .deb does not start the engine; run.sh starts it).
  if ! command -v kiri >/dev/null; then
    case "$(uname -m)" in
      aarch64) arch=arm64 sum=76fe00facf4d61aefe90fbb0e44774942b1507e810c4e7b47ea34fddafe0193d ;;
      *) arch=amd64 sum=5e7b1f6d8239aa590b314fd2ccc8f1472ba1427f414b35efa86ccedd6a6571b2 ;;
    esac
    pkg="$(mktemp -d)/kiri-lite.deb"
    curl -fsSL -o "$pkg" "https://github.com/libracoder/kiri/releases/download/v0.2.1/kiri-lite_0.2.1_${arch}.deb"
    echo "$sum  $pkg" | sha256sum -c --quiet
    sudo DEBIAN_FRONTEND=noninteractive apt-get install -y -qq "$pkg" >/dev/null
  fi
fi

echo "provisioned openpod-dev"
