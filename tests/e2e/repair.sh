#!/usr/bin/env bash
# A damaged file in the store, with real images: Alpine 3.21 and 3.22 share no layer but
# most of their files (the same contents, often with other times). A file a crash cut
# short shows in the image pulled first; `system verify` finds it and names both images;
# the next pull of the other image writes it again, in place; `--fix` removes the images
# that use a file whose damage keeps its size.
# shellcheck source=tests/e2e/lib.sh
. "$(dirname "$0")/lib.sh"

# Alpine 3.21.10 and 3.22, by digest: their tags move to new releases.
A=mirror.gcr.io/library/alpine@sha256:bc21487101ae47172509130ee1092cc273a831968d64b617776d1f69c81e42ab
B=mirror.gcr.io/library/alpine@sha256:49340b346798875b5592e5d420241f2f9cfba5f47e62ebb1446a1b34734e898f
pb pull "$A" >/dev/null
pb pull "$B" >/dev/null

object="$(object_of "$B" etc/profile)"
[ "$object" = "$(object_of "$A" etc/profile)" ] || fail "the two images do not share /etc/profile"
size="$(pb run --rm --network none "$B" sh -c 'wc -c < /etc/profile')"
[ "${size:-0}" -gt 0 ] || fail "cannot read /etc/profile"

# What a power cut right after a pull can leave: the name, without the data. The image
# pulled first has the object's time and reads it in both mount methods (with hard-link
# trees, the other image has a copy of its own when its time differs).
as_owner truncate -s 0 "$object"
[ "$(pb run --rm --network none "$A" sh -c 'wc -c < /etc/profile')" = 0 ] ||
  fail "the damage does not show in $A"
set +e
report="$(pb system verify)"
code=$?
set -e
[ "$code" = 1 ] || fail "system verify exited $code on a damaged store"
case "$report" in *"$A"*"$B"* | *"$B"*"$A"*) ;; *) fail "system verify did not name both images: $report" ;; esac
pass "system verify finds the damaged file and both images"

pb rmi "$B" >/dev/null
pb pull "$B" >/dev/null
for image in "$A" "$B"; do
  [ "$(pb run --rm --network none "$image" sh -c 'wc -c < /etc/profile')" = "$size" ] ||
    fail "$image still has the damaged /etc/profile after a new pull of $B"
done
pb system verify >/dev/null || fail "system verify still finds damage after the repair"
pass "the next pull repairs the file in both images"

# Damage that keeps the size: only reading the file finds it.
as_owner dd if=/dev/zero of="$object" bs=1 count=8 conv=notrunc status=none
pb system verify >/dev/null && fail "system verify missed damage of the same size"
pb system verify --fix >/dev/null || fail "system verify --fix failed"
[ -z "$(pb images | tail -n +2)" ] ||
  fail "system verify --fix kept an image that uses the damaged file: $(pb images)"
pb system verify >/dev/null || fail "damage is left after --fix"
pass "system verify --fix removes the images that use a damaged file"
