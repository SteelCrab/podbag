#!/bin/sh
# install.sh against local release files: it takes podbag-<arch>-linux-musl when the
# release has it, else podbag-<arch>-linux (older releases). A stand-in curl serves the
# files.
#
#   packaging/test/install-script.sh dist
set -eu

dist=$(cd "$1" && pwd)
src=$(cd "$(dirname "$0")/../.." && pwd)
arch=$(uname -m)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

# curl ... -o FILE URL: copy $RELEASE/<name> to FILE.
mkdir "$work/fake"
cat >"$work/fake/curl" <<'EOF'
#!/bin/sh
while [ $# -gt 1 ]; do
  [ "$1" = -o ] && out=$2
  shift
done
cp "$RELEASE/${1##*/}" "$out" 2>/dev/null || exit 22
EOF
chmod +x "$work/fake/curl"

# try NAME... : a release with these files; install.sh must take the first one.
try() {
  rel=$work/release-$1
  mkdir -p "$rel"
  for f in "$@"; do cp "$dist/podbag-$arch-linux-musl" "$rel/$f"; done
  (cd "$rel" && sha256sum -- * >SHA256SUMS)
  RELEASE=$rel PATH=$work/fake:$PATH PODBAG_VERSION=v0.0.0 PODBAG_INSTALL_DIR=$work/bin-$1 \
    PODBAG_SKIP_APPARMOR=1 sh "$src/install.sh" 2>"$work/log" || {
    cat "$work/log"
    exit 1
  }
  cat "$work/log"
  grep -q "^Downloading $1 " "$work/log"
  cmp "$work/bin-$1/podbag" "$dist/podbag-$arch-linux-musl"
}

try "podbag-$arch-linux-musl" "podbag-$arch-linux"
try "podbag-$arch-linux"
echo "install.sh: all good"
