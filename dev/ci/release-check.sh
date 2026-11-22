#!/bin/sh
# Check that a tag can become a release: a stable vX.Y.Z tag, the same version in the
# CLI, Podbag Desktop and its Tauri bundle, and a changelog section for it.
#   dev/ci/release-check.sh vX.Y.Z
set -eu
cd "$(dirname "$0")/../.."
[ "$#" -eq 1 ] || {
  echo "usage: dev/ci/release-check.sh vX.Y.Z" >&2
  exit 2
}
tag=$1
version=${tag#v}
fail() {
  echo "$*" >&2
  exit 1
}

printf '%s\n' "$tag" | grep -Eqx 'v[0-9]+\.[0-9]+\.[0-9]+' ||
  fail "$tag is not a stable release tag such as v1.2.3"

metadata=$(cargo metadata --locked --no-deps --format-version 1)
for package in podbag podbag-desktop; do
  have=$(printf '%s\n' "$metadata" |
    jq -er --arg p "$package" '.packages[] | select(.name == $p) | .version')
  [ "$have" = "$version" ] || fail "$package is version $have, but the tag is $tag"
done
have=$(jq -er .version desktop/tauri.conf.json)
[ "$have" = "$version" ] || fail "desktop/tauri.conf.json is version $have, but the tag is $tag"

dev/ci/release-notes.sh "$tag" > /dev/null
echo "$tag can be released"
