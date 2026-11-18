#!/bin/bash
# Build the openpod VM guest for macOS (run inside the lima VM: `dev/vm.sh run dev/macos/build-guest.sh`).
# Writes target/vm/{Image, initramfs.gz, disk.zst} in the repository.
set -euo pipefail
cd "$(dirname "$0")/../.."
repo=$(pwd)
version=6.18.64
work=${OPENPOD_GUEST_WORK:-$HOME/openpod-guest}
out=$repo/target/vm
mkdir -p "$work" "$out"

need=()
for tool in flex bison bc cpio zstd mkfs.ext4 gcc make; do command -v $tool >/dev/null || need+=($tool); done
[ -e /usr/include/libelf.h ] || need+=(libelf)
[ -e /usr/include/openssl/ssl.h ] || need+=(libssl)
if [ ${#need[@]} -gt 0 ]; then
  sudo -n apt-get install -y -q build-essential flex bison bc cpio zstd e2fsprogs libelf-dev libssl-dev >/dev/null
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
if ! cmp -s "$repo/dev/macos/kernel.config" .openpod-fragment; then
  make -s defconfig
  scripts/kconfig/merge_config.sh -m .config "$repo/dev/macos/kernel.config" >/dev/null
  make -s olddefconfig
  cp "$repo/dev/macos/kernel.config" .openpod-fragment
fi
# Every option of the fragment must have survived (a missing dependency drops one quietly).
missing=$(grep -E '^CONFIG_[A-Z0-9_]+=' "$repo/dev/macos/kernel.config" | while IFS= read -r line; do
  opt=${line%%=*}; want=${line#*=}
  have=$(grep -E "^$opt=" .config | cut -d= -f2- || true)
  if [ "$want" = n ]; then [ -z "$have" ] || echo "$opt=$have"; else [ "$have" = "$want" ] || echo "$opt (want $want, have ${have:-unset})"; fi
done)
[ -z "$missing" ] || { echo "kernel options not set:"; echo "$missing"; exit 1; }
make -s -j"$(nproc)" Image
cp arch/arm64/boot/Image "$out/Image"

# The static openpod for the guest.
target=aarch64-unknown-linux-musl
(cd "$repo" && env CC_aarch64_unknown_linux_musl=musl-gcc cargo build -q --release --target $target)
bin=$CARGO_TARGET_DIR/$target/release/openpod

# The initramfs: /init is openpod (it runs as `__vm-init` when it is PID 1).
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
dir /var/lib/openpod 755 0 0
dir /Users 755 0 0
file /init $bin 755 0 0
file /etc/passwd $work/passwd 644 0 0
file /etc/group $work/group 644 0 0
file /etc/hosts $work/hosts 644 0 0
LIST
printf 'root:x:0:0:root:/root:/init\n' > "$work/passwd"
printf 'root:x:0:\n' > "$work/group"
printf '127.0.0.1 localhost openpod\n::1 localhost\n' > "$work/hosts"
usr/gen_init_cpio "$list" | gzip -9 > "$out/initramfs.gz"

# The data disk: an empty ext4 file system, as non-zero blocks only (see src/vm/template.rs).
img=$work/disk.img
rm -f "$img"
truncate -s 64G "$img"
mkfs.ext4 -F -q -L openpod -E lazy_itable_init=1,lazy_journal_init=1,nodiscard "$img"
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
ls -l "$out"
