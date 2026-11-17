#!/usr/bin/env bash
# Download the latest Alpine minirootfs for this machine, verify it, and unpack it.
set -euo pipefail

dest="${1:?usage: dev/fetch-rootfs.sh <target-directory>}"
arch="$(uname -m)"
base="https://dl-cdn.alpinelinux.org/alpine/latest-stable/releases/$arch"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

curl -fsSL "$base/latest-releases.yaml" -o "$tmp/releases.yaml"

# Find the alpine-minirootfs entry and read its file name and sha256.
read -r file sum < <(awk '
  /^-/ { if (flavor == "alpine-minirootfs" && file != "") { print file, sha; exit } flavor = ""; file = ""; sha = "" }
  /flavor:/ { flavor = $2 }
  /file:/   { file = $2 }
  /sha256:/ { sha = $2 }
  END { if (flavor == "alpine-minirootfs" && file != "") print file, sha }
' "$tmp/releases.yaml")

if [ -z "${file:-}" ] || [ -z "${sum:-}" ]; then
  echo "cannot find alpine-minirootfs in $base/latest-releases.yaml" >&2
  exit 1
fi

curl -fsSL "$base/$file" -o "$tmp/$file"
echo "$sum  $tmp/$file" | sha256sum -c --quiet - || { echo "sha256 mismatch for $file" >&2; exit 1; }

mkdir -p "$dest"
# -p keeps modes as archived (1777 /tmp), not reduced by the umask.
tar -xpzf "$tmp/$file" -C "$dest"
echo "unpacked $file into $dest"
