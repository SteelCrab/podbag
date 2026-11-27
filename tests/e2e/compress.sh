#!/usr/bin/env bash
# Image compression with real images: `system compress` compresses the store's objects,
# and the images still work wherever they are read: a container sees the files the
# reference engine sees, `system verify` checks the compressed objects, `save` writes
# an archive the reference engine loads, and a build reads its base images. With
# compression on, pull compresses idle images by itself; an image a container uses
# stays plain; `system decompress` makes the objects plain again; removing the images
# leaves no compressed object.
# shellcheck source=tests/e2e/lib.sh
. "$(dirname "$0")/lib.sh"

# Alpine 3.21.10 and 3.22 (as in repair.sh), and python:3.13-slim (Python 3.13.18,
# Debian trixie; a larger image with many files), by digest: their tags move to new
# releases.
A=mirror.gcr.io/library/alpine@sha256:bc21487101ae47172509130ee1092cc273a831968d64b617776d1f69c81e42ab
B=mirror.gcr.io/library/alpine@sha256:49340b346798875b5592e5d420241f2f9cfba5f47e62ebb1446a1b34734e898f
P=mirror.gcr.io/library/python@sha256:7249d5587b78209a006dc4a3b5340678f84ea729239193aa559962cd8c1fce50
SAVED=localhost/podbag-e2e-saved:1
BUILT=localhost/podbag-e2e-compress:1
# A large file of the python image: its object is the one the checks look at.
LIB=usr/local/lib/libpython3.13.so.1.0

# How many compressed objects the store has. Without an objects folder it fails: a wrong
# path must not count as "none". Assign its output before a test (`n="$(zst_count)"`), so
# that set -e ends the script when it fails.
zst_count() {
  local objects="$PODBAG_DATA_DIR/store/objects"
  as_owner test -d "$objects" || fail "there is no objects folder: $objects"
  as_owner find "$objects" -name '*.zst' | wc -l
}
# A column of a row of `system df`.
df_cell() { pb system df | awk -v t="$1" -v c="$2" '$1 == t { print $c }'; }
# Bytes of a size as `system df` prints it (7.6MB).
bytes() {
  awk -v s="$1" 'BEGIN {
    u = s; sub(/^[0-9.]+/, "", u)
    m["B"] = 1; m["kB"] = 1e3; m["MB"] = 1e6; m["GB"] = 1e9; m["TB"] = 1e12
    if (!(u in m)) exit 1
    printf "%.0f\n", (s + 0) * m[u]
  }'
}
compressed() { as_owner test -f "$1.zst" && ! as_owner test -e "$1"; }
plain() { as_owner test -f "$1" && ! as_owner test -e "$1.zst"; }

# --- compress ---------------------------------------------------------------------------

pb pull "$A" >/dev/null
pb pull "$B" >/dev/null
pb pull "$P" >/dev/null
objects_before="$(bytes "$(df_cell Objects 3)")"
count="$(zst_count)"
[ "$count" = 0 ] || fail "objects are compressed with compression off"

[ "$(pb system compression idle --after 0 --json)" = '{"mode":"idle","after":0}' ] ||
  fail "system compression did not save: $(pb system compression --json)"
out="$(pb system compress)"
case "$out" in "Compressed 3 images: "*) ;; *) fail "system compress printed: $out" ;; esac
count="$(df_cell Compressed 2)"
[ "${count:-0}" -gt 0 ] || fail "system df shows no compressed objects: $(pb system df)"
count="$(zst_count)"
[ "$count" -gt 0 ] || fail "the store has no compressed objects"
objects_after="$(bytes "$(df_cell Objects 3)")"
[ "$objects_after" -lt "$objects_before" ] ||
  fail "the objects did not get smaller: $objects_before -> $objects_after bytes"
lib="$(object_of "$P" "$LIB")"
compressed "$lib" || fail "/$LIB is not compressed"
pass "system compress compresses the images ($objects_before -> $objects_after bytes)"

# --- a container ------------------------------------------------------------------------

# Every file of the image (its contents) and some files' modes, owners and times, as a
# container sees them. The engines' own files in /etc are left out.
list_script='
find /usr /etc /lib -xdev -type f ! -path /etc/hosts ! -path /etc/hostname \
  ! -path /etc/resolv.conf -print0 | LC_ALL=C sort -z | xargs -0 sha256sum
'
stat_script='
stat -c "%a %u %g %Y %n" /etc/passwd /etc/shadow /usr/bin/passwd /usr/bin/dash \
  /usr/lib/os-release /usr/local/bin/python3.13 /usr/local/lib/libpython3.13.so.1.0 \
  /usr/local/lib/python3.13/json/__init__.py
'
ref pull -q "$P" >/dev/null
ref run --rm --network none "$P" sh -c "$list_script" >"$E2E_TMP/ref-files"
pb run --rm --network none "$P" sh -c "$list_script" >"$E2E_TMP/pb-files"
[ "$(wc -l <"$E2E_TMP/ref-files")" -gt 1000 ] ||
  fail "the reference engine lists only $(wc -l <"$E2E_TMP/ref-files") files: the test is wrong"
cmp -s "$E2E_TMP/ref-files" "$E2E_TMP/pb-files" ||
  fail "files differ from the reference engine's: $(diff "$E2E_TMP/ref-files" "$E2E_TMP/pb-files" | head -5)"
want="$(ref run --rm --network none "$P" sh -c "$stat_script")"
got="$(pb run --rm --network none "$P" sh -c "$stat_script")"
[ "$got" = "$want" ] || fail "modes, owners or times differ: '$got' (want '$want')"
pb run --rm --network none "$P" python3 -c 'import json, sqlite3, ssl' ||
  fail "python cannot import json, sqlite3 and ssl"
pass "a container of a compressed image has the reference engine's files"

# --- verify -----------------------------------------------------------------------------

# The container made the python image plain: compress it again.
pb system compress >/dev/null
compressed "$lib" || fail "/$LIB is not compressed again"
pb system verify >/dev/null || fail "system verify failed on compressed objects: $(pb system verify)"
pass "system verify passes with compressed objects"

# --- save -------------------------------------------------------------------------------

# Save reads the compressed objects as they are; the reference engine gets layers it
# never saw (podbag writes them again).
pb tag "$P" "$SAVED"
E2E_BUILT+=("$SAVED")
pb save "$SAVED" | ref load >/dev/null
compressed "$lib" || fail "save made /$LIB plain"
ref run --rm --network none "$SAVED" sh -c "$list_script" >"$E2E_TMP/saved-files"
cmp -s "$E2E_TMP/ref-files" "$E2E_TMP/saved-files" ||
  fail "the saved image's files differ: $(diff "$E2E_TMP/ref-files" "$E2E_TMP/saved-files" | head -5)"
ref run --rm --network none "$SAVED" python3 -c 'import json, sqlite3, ssl' ||
  fail "python of the saved image does not run in the reference engine"
pass "save of a compressed image loads and runs in the reference engine"

# --- build ------------------------------------------------------------------------------

pb system compress >/dev/null
for image in "$A" "$B"; do
  compressed "$(object_of "$image" bin/busybox)" || fail "$image is not compressed before the build"
done
ctx="$(mktemp -d "$E2E_TMP/build.XXXX")"
echo e2e >"$ctx/f"
cat >"$ctx/Dockerfile" <<EOF
FROM $A
COPY --chown=nobody:nobody f /f
COPY --from=$B /etc/os-release /b-release
EOF
pb build -q -t "$BUILT" "$ctx" >/dev/null || fail "the build from compressed images failed"
[ "$(pb run --rm --network none "$BUILT" stat -c %U:%G /f)" = nobody:nobody ] ||
  fail "/f of the built image does not belong to nobody"
release="$(pb run --rm --network none "$BUILT" cat /b-release)"
case "$release" in *VERSION_ID=3.22*) ;; *) fail "/b-release is not Alpine 3.22's: $release" ;; esac
[ "$release" = "$(pb run --rm --network none "$B" cat /etc/os-release)" ] ||
  fail "/b-release differs from /etc/os-release of $B"
pass "a build reads compressed base images"

# --- automatic compression --------------------------------------------------------------

# With compression on and --after 0, a pull compresses every image no container uses
# (the pulled one excepted), within its budget.
pb rmi "$B" >/dev/null
pb system decompress "$A" "$P" "$BUILT" >/dev/null
before="$(zst_count)"
# One pull's freeze stops at its 2 second budget. Its first batch only removes an image's
# artifact, and on a slow runner deleting that tree can use the rest of the budget. The
# next pull goes on where the last one stopped.
after="$before"
for _ in 1 2 3; do
  pb pull "$B" >/dev/null || fail "the pull failed with compression on"
  after="$(zst_count)"
  [ "$after" -gt "$before" ] && break
done
[ "$after" -gt "$before" ] || fail "three pulls compressed nothing: $before -> $after compressed objects"
pass "pull compresses idle images ($before -> $after compressed objects)"

# --- in use -----------------------------------------------------------------------------

pb system decompress "$A" >/dev/null
busybox="$(object_of "$A" bin/busybox)"
pb create --name holder "$A" >/dev/null
before="$(zst_count)"
pb system compress "$A" 2>"$E2E_TMP/err" >/dev/null || fail "system compress failed: $(cat "$E2E_TMP/err")"
grep -q "is not compressed: a container uses it" "$E2E_TMP/err" ||
  fail "system compress did not say the image is in use: $(cat "$E2E_TMP/err")"
after="$(zst_count)"
[ "$after" = "$before" ] || fail "system compress compressed objects of an image in use"
plain "$busybox" || fail "/bin/busybox of the image in use is compressed"
pb rm holder >/dev/null
pass "system compress skips an image a container uses"

# --- decompress -------------------------------------------------------------------------

pb system compress "$P" >/dev/null
compressed "$lib" || fail "/$LIB is not compressed"
out="$(pb system decompress "$P")"
[ "$out" = "Decompressed 1 images" ] || fail "system decompress printed: $out"
plain "$lib" || fail "/$LIB is still compressed"
pass "system decompress makes the objects plain"

# --- clean-up ---------------------------------------------------------------------------

pb rmi "$A" "$B" "$P" "$SAVED" "$BUILT" >/dev/null
pb system prune -f >/dev/null
[ -z "$(pb images | tail -n +2)" ] || fail "images are left: $(pb images)"
left="$(zst_count)"
[ "$left" = 0 ] || fail "$left compressed objects are left"
pb system verify >/dev/null || fail "system verify failed on the empty store: $(pb system verify)"
pass "removing the images leaves no compressed object"
