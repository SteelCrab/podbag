#!/usr/bin/env bash
# Stage the upstream binaries used by the managed BuildKit integration suite.
set -euo pipefail

destination=$1
buildkit_version=v0.33.1
buildx_version=v0.38.1
rootlesskit_version=v3.2.0
runc_version=v1.5.2

machine=$(uname -m)
case "$machine" in
  x86_64)
    arch=amd64
    buildkit_sum=4e044bcd62a0c0bbe6a8c94d73989de2bfe4c04dbc0f9d6021cf96b72cd1d965
    buildx_sum=e25d5cd675092bb5d1d03632fdc46cdf3fede7b514dc5f023a105e00d3ce5ad4
    rootlesskit_sum=ff612d1d35854a52569acc4a4e9152f41504e69dcc13ff4c94cc9040dc28b3d9
    runc_sum=599f6f94ff8c5057241eff0d54c3c74f95c34935b6457b33fe545defc61e9488
    ;;
  aarch64)
    arch=arm64
    buildkit_sum=4e1ba91f139761f249a1fa6c71e632dfdbdafeda416176ecadefbb95225e56c4
    buildx_sum=3b83b3731916ba433feccaaa689fe5bcdc81c641d92c8e6c2ea865dfa141c97a
    rootlesskit_sum=d68815112d658affe45e1ea00a9016758ba7e92230c1e69f7ee572d053e6f9c0
    runc_sum=d10ecae898361832a059be2089bab92d158aec54661b18ed7346ed79628b46b0
    ;;
  *) echo "Unsupported BuildKit test architecture: $machine" >&2; exit 1 ;;
esac

downloads=$(mktemp -d)
trap 'rm -rf "$downloads"' EXIT

fetch() {
  local url=$1 checksum=$2 file=$3
  curl --fail --silent --show-error --location --retry 3 --connect-timeout 10 "$url" -o "$downloads/$file"
  (cd "$downloads" && printf '%s  %s\n' "$checksum" "$file" | sha256sum --check --strict)
}

upstream=https://github.com
fetch "$upstream/moby/buildkit/releases/download/$buildkit_version/buildkit-$buildkit_version.linux-$arch.tar.gz" "$buildkit_sum" buildkit.tar.gz
fetch "$upstream/docker/buildx/releases/download/$buildx_version/buildx-$buildx_version.linux-$arch" "$buildx_sum" buildx
fetch "$upstream/rootless-containers/rootlesskit/releases/download/$rootlesskit_version/rootlesskit-$machine.tar.gz" "$rootlesskit_sum" rootlesskit.tar.gz
fetch "$upstream/opencontainers/runc/releases/download/$runc_version/runc.$arch" "$runc_sum" runc

# The rootful worker's bridge uses the CNI binaries from the same release.
tar -xzf "$downloads/buildkit.tar.gz" -C "$downloads" \
  bin/{buildctl,buildkitd,buildkit-cni-{bridge,firewall,host-local,loopback}}
tar -xzf "$downloads/rootlesskit.tar.gz" -C "$downloads" rootlesskit
mkdir -p "$destination"
install -m 0755 "$downloads/bin/"* "$downloads/"{buildx,rootlesskit,runc} "$destination"

"$destination/buildkitd" --version
"$destination/buildx" version
"$destination/rootlesskit" --version
"$destination/runc" --version
