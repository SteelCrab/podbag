#!/usr/bin/env bash
# Exercise selection and the stock NVIDIA driver hook on a runner without a GPU.
set -euo pipefail
bin=$(realpath "${1:?path to podbag}")
rootfs=$(realpath "${2:?path to the test root filesystem}")
arch=$(dpkg --print-architecture)
case "$arch" in
    amd64) checksum=a6bd7f3a94b0b53b9748872709e70e810473081de0e836a59405073012c49368 ;;
    arm64) checksum=486db4677eb48f1b25a5092cd13b0523aaf03f63cf0411c6922a33bb35eb4fef ;;
    *) echo "Unsupported test architecture: $arch" >&2; exit 1 ;;
esac
fixture=$(mktemp -d)
trap 'rm -rf "$fixture"' EXIT
url=https://nvidia.github.io/libnvidia-container/stable/deb/$arch/nvidia-container-toolkit-base_1.20.2-1_$arch.deb
curl --fail --silent --show-error --location --retry 3 "$url" -o "$fixture/toolkit.deb"
echo "$checksum  $fixture/toolkit.deb" | sha256sum --check
dpkg-deb -x "$fixture/toolkit.deb" "$fixture/toolkit"
cp "$(dirname "$0")/fixtures/nvidia-ctk" "$fixture/nvidia-ctk"
mkdir "$fixture/runtime"
# Keep synthetic device nodes, driver files and processes inside this test.
sudo unshare --mount --propagation private --net --pid --fork --kill-child --mount-proc \
    bash -euo pipefail -s -- "$bin" "$rootfs" "$fixture" <<'PROBE'
bin=$1
rootfs=$2
fixture=$3
scratch="$fixture/runtime"
mount -t tmpfs tmpfs "$scratch"
for name in usr dev; do
    mkdir "$scratch/$name-upper" "$scratch/$name-work"
    mount -t overlay overlay -o "lowerdir=/$name,upperdir=$scratch/$name-upper,workdir=$scratch/$name-work" "/$name"
done
install -m 755 "$fixture/toolkit/usr/bin/nvidia-cdi-hook" /usr/bin/nvidia-cdi-hook
install -m 755 "$fixture/nvidia-ctk" /usr/bin/nvidia-ctk
mknod /dev/nvidia0 c 195 0
mknod /dev/nvidia1 c 195 1
mknod /dev/nvidiactl c 195 255
mkdir -p /usr/lib/podbag-nvidia-fixture
printf 'int podbag_fixture(void) { return 7; }\n' |
    cc -nostdlib -shared -Wl,-soname,libcuda.so.1 -x c - -o /usr/lib/podbag-nvidia-fixture/libcuda.so.1.0.0
cp -a "$rootfs" "$scratch/rootfs"
export PODBAG_DATA_DIR="$scratch/data"
export PODBAG_RUNTIME_DIR="$scratch/run"
export PODBAG_CONFIG_DIR="$scratch/config"
export PODBAG_KEEPER=0
export NVIDIA_VISIBLE_DEVICES=host-fixture-blocked
# The container shell expands the variables and checks its own device nodes.
# shellcheck disable=SC2016
timeout 35s "$bin" run --rm --network none --read-only --tmpfs /writable --gpus 'driver=nvidia,device=GPU-B' \
    --env NVIDIA_VISIBLE_DEVICES=all --rootfs "$scratch/rootfs" -- sh -ec '
test -c /dev/nvidia1
test -c /dev/nvidiactl
test ! -e /dev/nvidia0
test "$(stat -c %t:%T /dev/nvidia1)" = c3:1
test "$(readlink /usr/lib/podbag-nvidia-fixture/libcuda.so.1)" = libcuda.so.1.0.0
test -s /usr/lib/podbag-nvidia-fixture/libcuda.so.1
test "$NVIDIA_VISIBLE_DEVICES" = void
test "$NVIDIA_CTK_LIBCUDA_DIR" = /usr/lib/podbag-nvidia-fixture
touch /writable/probe
if touch /usr/lib/podbag-nvidia-fixture/libcuda.so.1.0.0 2>/dev/null; then exit 1; fi
'
test ! -e /usr/lib/podbag-nvidia-fixture/libcuda.so.1
printf 'NVIDIA Toolkit 1.20.2 hooks linked the mounted CUDA fixture. Only the selected and control devices were present.\n'

tar -C "$rootfs" -cf "$scratch/image.tar" .
"$bin" image import "$scratch/image.tar" podbag-nvidia-fixture:local >/dev/null
socket="$scratch/api.sock"
"$bin" system service --socket "$socket" --timeout 0 > "$scratch/api.log" 2>&1 &
api_pid=$!
trap 'kill "$api_pid" 2>/dev/null || true; wait "$api_pid" 2>/dev/null || true' EXIT
attempt=0
while ! test -S "$socket" && test "$attempt" -lt 40; do
    sleep .05
    attempt=$((attempt+1))
done
docker_cmd() { docker --host "unix://$socket" "$@"; }
container=$(docker_cmd create --network none --read-only --tmpfs /writable \
    --gpus 'driver=nvidia,device=GPU-B' podbag-nvidia-fixture:local sh -ec '
    test -c /dev/nvidia1 && test ! -e /dev/nvidia0 &&
    test -s /usr/lib/podbag-nvidia-fixture/libcuda.so.1 &&
    touch /writable/probe
')
requests=$(docker_cmd inspect --format '{{range .HostConfig.DeviceRequests}}{{.Driver}}={{join .DeviceIDs ","}};{{end}}' "$container")
test "$requests" = "nvidia=GPU-B;"
docker_cmd start -a "$container"
docker_cmd rm "$container" > /dev/null
printf 'Docker API preserved the request and exposed only the selected NVIDIA GPU on start.\n'
PROBE
