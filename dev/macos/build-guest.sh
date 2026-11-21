#!/bin/bash
# Build the podbag VM guest for macOS on Linux of the Mac's CPU: arm64 for Apple silicon
# (the lima VM: `dev/vm.sh run dev/macos/build-guest.sh`), x86_64 for Intel Macs.
# Writes target/vm/{Image, Image.zst, initramfs.gz, disk.zst, arch} in the repository.
set -euo pipefail
cd "$(dirname "$0")/../.."
repo=$(pwd)
version=6.18.64
work=${PODBAG_GUEST_WORK:-$HOME/podbag-guest}
out=$repo/target/vm
mkdir -p "$work" "$out"

# The guest's CPU is this machine's. Virtualization.framework boots an arm64 Image on Apple
# silicon and a bzImage on an Intel Mac; both are written as `Image`.
arch=$(uname -m)
case $arch in
  aarch64) kernel_arch=arm64 kernel_target=Image cpu_config=kernel-arm64.config ;;
  x86_64) kernel_arch=x86 kernel_target=bzImage cpu_config=kernel-x86_64.config ;;
  *) echo "no podbag VM guest for $arch" >&2; exit 1 ;;
esac
fragments=("$repo/dev/macos/kernel.config" "$repo/dev/macos/$cpu_config")

need=()
for tool in flex bison bc cpio zstd mkfs.ext4 gcc make; do command -v $tool >/dev/null || need+=($tool); done
[ -e /usr/include/libelf.h ] || need+=(libelf)
[ -e /usr/include/openssl/ssl.h ] || need+=(libssl)
command -v musl-gcc >/dev/null || need+=(musl-gcc)
if [ ${#need[@]} -gt 0 ]; then
  sudo -n apt-get install -y -q build-essential flex bison bc cpio zstd e2fsprogs libelf-dev libssl-dev musl-tools >/dev/null
fi

# The kernel, checked against kernel.org's list of sha256 sums.
src=$work/linux-$version
if [ ! -d "$src" ]; then
  cd "$work"
  curl -fsSLO https://cdn.kernel.org/pub/linux/kernel/v6.x/linux-$version.tar.xz
  curl -fsSL https://cdn.kernel.org/pub/linux/kernel/v6.x/sha256sums.asc -o sha256sums.asc
  grep " linux-$version.tar.xz\$" sha256sums.asc | sha256sum -c -
  tar -xf linux-$version.tar.xz
  rm linux-$version.tar.xz
fi
cd "$src"
fragment=$work/fragment-$arch.config
cat "${fragments[@]}" > "$fragment"
if ! cmp -s "$fragment" .podbag-fragment; then
  make -s defconfig
  scripts/kconfig/merge_config.sh -m .config "${fragments[@]}" >/dev/null
  make -s olddefconfig
  cp "$fragment" .podbag-fragment
fi
# Every option of the fragments must have survived (a missing dependency drops one quietly).
missing=$(grep -hE '^CONFIG_[A-Z0-9_]+=' "${fragments[@]}" | while IFS= read -r line; do
  opt=${line%%=*}; want=${line#*=}
  have=$(grep -E "^$opt=" .config | cut -d= -f2- || true)
  if [ "$want" = n ]; then [ -z "$have" ] || echo "$opt=$have"; else [ "$have" = "$want" ] || echo "$opt (want $want, have ${have:-unset})"; fi
done)
[ -z "$missing" ] || { echo "kernel options not set:"; echo "$missing"; exit 1; }
make -s -j"$(nproc)" "$kernel_target"
cp "arch/$kernel_arch/boot/$kernel_target" "$out/Image"
# What a release build of podbag carries (`--features embed-guest`).
zstd -q -19 -T0 -f "$out/Image" -o "$out/Image.zst"

# The static podbag for the guest.
target=$arch-unknown-linux-musl
rustup target list --installed | grep -qx "$target" || rustup target add "$target"
(cd "$repo" && env "CC_${target//-/_}=musl-gcc" cargo build -q --release --target "$target")
bin=${CARGO_TARGET_DIR:-$repo/target}/$target/release/podbag

# The initramfs: /init is podbag (it runs as `__vm-init` when it is PID 1).
list=$work/initramfs.list
cat > "$list" <<LIST
dir /dev 755 0 0
nod /dev/console 600 0 0 c 5 1
nod /dev/null 666 0 0 c 1 3
dir /proc 755 0 0
dir /sys 755 0 0
dir /run 755 0 0
dir /tmp 1777 0 0
dir /root 700 0 0
dir /etc 755 0 0
dir /var 755 0 0
dir /var/lib 755 0 0
dir /var/lib/podbag 755 0 0
dir /Users 755 0 0
file /init $bin 755 0 0
file /etc/passwd $work/passwd 644 0 0
file /etc/group $work/group 644 0 0
file /etc/hosts $work/hosts 644 0 0
LIST
printf 'root:x:0:0:root:/root:/init\n' > "$work/passwd"
printf 'root:x:0:\n' > "$work/group"
printf '127.0.0.1 localhost podbag\n::1 localhost\n' > "$work/hosts"
usr/gen_init_cpio "$list" | gzip -9 > "$out/initramfs.gz"

# The data disk: an empty ext4 file system, as non-zero blocks only (see src/vm/template.rs).
img=$work/disk.img
rm -f "$img"
truncate -s 64G "$img"
mkfs.ext4 -F -q -L podbag -E lazy_itable_init=1,lazy_journal_init=1,nodiscard "$img"
python3 - "$img" "$work/disk.raw" <<'PY'
import os, struct, sys
src, dst = sys.argv[1], sys.argv[2]
size = os.path.getsize(src)
block = 4096
with open(src, 'rb') as f, open(dst, 'wb') as out:
    out.write(b'OPDISK1\n' + struct.pack('<Q', size))
    fd = f.fileno()
    off = 0
    while off < size:
        try:
            start = os.lseek(fd, off, os.SEEK_DATA)
        except OSError:
            break
        end = os.lseek(fd, start, os.SEEK_HOLE)
        pos = start
        while pos < end:
            f.seek(pos)
            data = f.read(min(1 << 20, end - pos))
            # Non-zero runs of 4 KiB blocks.
            i = 0
            while i < len(data):
                b = data[i:i + block]
                if b.count(0) == len(b):
                    i += block
                    continue
                j = i
                while j < len(data) and data[j:j + block].count(0) != len(data[j:j + block]):
                    j += block
                out.write(struct.pack('<QI', pos + i, len(data[i:j])) + data[i:j])
                i = j
            pos += len(data)
        off = end
PY
zstd -q -19 -f "$work/disk.raw" -o "$out/disk.zst"
rm -f "$img" "$work/disk.raw"
# What `cargo build --features embed-guest` checks against the Mac program's CPU.
echo "$arch" > "$out/arch"
ls -l "$out"
