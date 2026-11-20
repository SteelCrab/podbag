#!/bin/sh
# Install podbag from its GitHub releases (Linux x86_64 and arm64, macOS on Apple silicon):
#
#   curl -fsSL https://podbag.dev/install.sh | sh
#
# Settings:
#   PODBAG_VERSION=v0.2.0      install this release (default: the latest)
#   PODBAG_INSTALL_DIR=DIR     install into DIR (default: /usr/local/bin; sudo when needed)
#   PODBAG_SKIP_APPARMOR=1     do not add the AppArmor profile (Ubuntu 24.04 and later)
set -eu

repo=SteelCrab/podbag
version=${PODBAG_VERSION:-latest}
dir=${PODBAG_INSTALL_DIR:-/usr/local/bin}

say() { printf '%s\n' "$*" >&2; }
fail() {
  say "podbag install: $*"
  exit 1
}

# 1. The file for this system.
os=$(uname -s)
arch=$(uname -m)
case $arch in
  x86_64 | amd64) arch=x86_64 ;;
  aarch64 | arm64) arch=aarch64 ;;
  *) fail "there is no podbag build for this CPU ($arch)" ;;
esac
case $os in
  Linux) file=podbag-$arch-linux ;;
  Darwin)
    [ "$arch" = aarch64 ] || fail "on a Mac, podbag needs Apple silicon"
    file=podbag-aarch64-macos
    ;;
  *) fail "no installer for $os; on Windows, download podbag.exe from https://github.com/$repo/releases" ;;
esac
case $version in
  latest) url=https://github.com/$repo/releases/latest/download ;;
  v*) url=https://github.com/$repo/releases/download/$version ;;
  *) url=https://github.com/$repo/releases/download/v$version ;;
esac

# 2. Download the program and the release's checksums.
if command -v curl >/dev/null 2>&1; then
  get() { curl -fsSL --proto '=https' --tlsv1.2 -o "$2" "$1"; }
elif command -v wget >/dev/null 2>&1; then
  get() { wget -q -O "$2" "$1"; }
else
  fail "curl or wget is needed"
fi
tmp=$(mktemp -d)
staged=
install_sudo=
cleanup() {
  if [ -n "$staged" ]; then $install_sudo rm -f "$staged"; fi
  rm -rf "$tmp"
}
trap cleanup EXIT
trap 'exit 1' INT TERM
say "Downloading $file ($version)"
get "$url/$file" "$tmp/podbag" || fail "cannot download $url/$file"
get "$url/SHA256SUMS" "$tmp/SHA256SUMS" || fail "cannot download $url/SHA256SUMS"

# 3. Check it.
want=$(awk -v f="$file" '$2 == f { print $1 }' "$tmp/SHA256SUMS")
[ -n "$want" ] || fail "SHA256SUMS has no line for $file"
if command -v sha256sum >/dev/null 2>&1; then
  got=$(sha256sum "$tmp/podbag" | cut -d ' ' -f 1)
else
  got=$(shasum -a 256 "$tmp/podbag" | cut -d ' ' -f 1)
fi
[ "$got" = "$want" ] || fail "the download does not match SHA256SUMS"

# 4. Install it, with sudo when the directory needs it.
if [ "$(id -u)" -ne 0 ] && ! { mkdir -p "$dir" 2>/dev/null && [ -w "$dir" ]; }; then
  command -v sudo >/dev/null 2>&1 || fail "$dir is not writable; set PODBAG_INSTALL_DIR"
  install_sudo=sudo
fi
$install_sudo mkdir -p "$dir"
[ ! -d "$dir/podbag" ] || fail "$dir/podbag is a directory"
staged=$($install_sudo mktemp "$dir/.podbag.XXXXXXXX")
$install_sudo install -m 0755 "$tmp/podbag" "$staged"
installed_version=$("$staged" --version) || fail "cannot run the downloaded $file"
$install_sudo mv -f "$staged" "$dir/podbag"
staged=
say "Installed $dir/podbag ($installed_version)"

# 5. Ubuntu 24.04 and later allow user namespaces (rootless containers) only to programs
#    with an AppArmor profile.
if [ "$os" = Linux ] && [ "${PODBAG_SKIP_APPARMOR:-0}" != 1 ] &&
  [ "$(cat /proc/sys/kernel/apparmor_restrict_unprivileged_userns 2>/dev/null)" = 1 ] &&
  [ -d /etc/apparmor.d ]; then
  sudo=
  [ "$(id -u)" -eq 0 ] || sudo=sudo
  say "Adding an AppArmor profile, so that podbag runs without root"
  profile="$tmp/podbag.apparmor"
  "$dir/podbag" check --apparmor-profile > "$profile" || fail "cannot generate the AppArmor profile"
  $sudo apparmor_parser -r "$profile" || fail "cannot load the AppArmor profile"
  $sudo install -m 0644 "$profile" /etc/apparmor.d/podbag
fi

case ":$PATH:" in
  *":$dir:"*) ;;
  *) say "Add $dir to your PATH to run podbag by name." ;;
esac
if [ "$os" = Linux ]; then
  say "Next: podbag check    (what this system still needs, and how to add it)"
else
  say "Next: podbag run --rm alpine echo hello"
fi
