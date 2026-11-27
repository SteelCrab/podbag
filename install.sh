#!/bin/sh
# Install podbag from its GitHub releases (Linux x86_64 and arm64, macOS on Apple silicon
# and Intel):
#
#   curl -fsSL https://podbag.dev/install.sh | sh
#
# Podbag Desktop on a Mac (the app, with the CLI in it):
#
#   curl -fsSL https://podbag.dev/install.sh | PODBAG_DESKTOP=1 sh
#
# The release files are not notarized. curl and wget do not mark what they download as
# quarantined, so macOS opens what this script installs without a warning.
#
# Settings:
#   PODBAG_VERSION=v0.3.2      install this release (default: the latest)
#   PODBAG_INSTALL_DIR=DIR     install the CLI into DIR (default: /usr/local/bin; sudo when needed)
#   PODBAG_DESKTOP=1           install Podbag Desktop instead (macOS)
#   PODBAG_APP_DIR=DIR         install Podbag Desktop into DIR (default: /Applications)
#   PODBAG_SKIP_APPARMOR=1     do not add the AppArmor profile (Ubuntu 24.04 and later)
set -eu

repo=SteelCrab/podbag
version=${PODBAG_VERSION:-latest}
dir=${PODBAG_INSTALL_DIR:-/usr/local/bin}
desktop=${PODBAG_DESKTOP:-0}

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
  Linux)
    [ "$desktop" != 1 ] ||
      fail "on Linux, install Podbag Desktop from the .deb or .rpm at https://github.com/$repo/releases/latest"
    file=podbag-$arch-linux
    ;;
  Darwin)
    # A shell that Rosetta runs says x86_64 on Apple silicon; the VM needs the Mac's CPU.
    [ "$(sysctl -n sysctl.proc_translated 2>/dev/null)" != 1 ] || arch=aarch64
    if [ "$desktop" = 1 ]; then
      file=podbag-desktop-$arch-macos.zip
    else
      file=podbag-$arch-macos
    fi
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
  if [ -n "$staged" ]; then $install_sudo rm -rf "$staged"; fi
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

# 4. Podbag Desktop: unpack the app, check that its podbag runs, then put it in place of
#    the old one (the same way as the app's own updates).
if [ "$desktop" = 1 ]; then
  appdir=${PODBAG_APP_DIR:-/Applications}
  app=$appdir/podbag.app
  ditto -x -k "$tmp/podbag" "$tmp/unpacked" || fail "cannot unpack $file"
  [ -d "$tmp/unpacked/podbag.app" ] || fail "$file has no podbag.app"
  installed_version=$("$tmp/unpacked/podbag.app/Contents/MacOS/podbag" --version) ||
    fail "cannot run the podbag in $file"
  if [ "$(id -u)" -ne 0 ] && ! { mkdir -p "$appdir" 2>/dev/null && [ -w "$appdir" ]; }; then
    command -v sudo >/dev/null 2>&1 || fail "$appdir is not writable; set PODBAG_APP_DIR"
    install_sudo=sudo
  fi
  $install_sudo mkdir -p "$appdir"
  # Stage in the same folder, so the swap is two renames.
  staged=$($install_sudo mktemp -d "$appdir/.podbag.XXXXXXXX")
  $install_sudo mv "$tmp/unpacked/podbag.app" "$staged/podbag.app"
  if [ -e "$app" ]; then $install_sudo mv "$app" "$staged/old.app"; fi
  $install_sudo mv "$staged/podbag.app" "$app"
  say "Installed $app ($installed_version)"
  say "Next: open $app    (quit podbag first if it is running)"
  say "      It offers to install the podbag command for the terminal."
  exit 0
fi

# 5. Install the CLI, with sudo when the directory needs it.
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

# 6. Ubuntu 24.04 and later allow user namespaces (rootless containers) only to programs
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
