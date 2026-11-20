# shellcheck shell=bash
# Shared by the end-to-end tests: real images from a registry, and podbag compared with
# a reference engine (docker when its daemon answers, else podman).
#
#   PODBAG=<binary>      the podbag to test (default: podbag on PATH)
#   PODBAG_E2E_ROOT=1    run podbag as root, through sudo (default: rootless)
#
# Each test gets data, runtime and config directories of its own and removes them.
set -euo pipefail

PODBAG="${PODBAG:-$(command -v podbag)}"
E2E_ROOT="${PODBAG_E2E_ROOT:-}"
E2E_TMP="$(mktemp -d)"
export PODBAG_DATA_DIR="$E2E_TMP/data"
export PODBAG_RUNTIME_DIR="$E2E_TMP/run"
export PODBAG_CONFIG_DIR="$E2E_TMP/config"
export PODBAG_KEEPER_LINGER_MS=300

if docker info >/dev/null 2>&1; then
  REF=(docker)
elif command -v podman >/dev/null 2>&1; then
  REF=(podman)
else
  echo "no reference engine: install docker or podman" >&2
  exit 1
fi

mode() { if [ "$E2E_ROOT" = 1 ]; then echo root; else echo rootless; fi; }

# podbag in the chosen mode.
pb() {
  if [ "$E2E_ROOT" = 1 ]; then
    sudo -n env PODBAG_DATA_DIR="$PODBAG_DATA_DIR" PODBAG_RUNTIME_DIR="$PODBAG_RUNTIME_DIR" \
      PODBAG_CONFIG_DIR="$PODBAG_CONFIG_DIR" PODBAG_KEEPER_LINGER_MS="$PODBAG_KEEPER_LINGER_MS" \
      "$PODBAG" "$@"
  else
    "$PODBAG" "$@"
  fi
}

# A command as the owner of podbag's store: root in root mode.
as_owner() {
  if [ "$E2E_ROOT" = 1 ]; then sudo -n "$@"; else "$@"; fi
}

ref() { "${REF[@]}" "$@"; }

pass() { echo "ok    [$(mode)] $*"; }
fail() {
  echo "FAIL  [$(mode)] $*" >&2
  exit 1
}

# Build an image from the Dockerfile on stdin with the reference engine, and load it
# into podbag.
build_and_load() {
  local name="$1" ctx
  ctx="$(mktemp -d "$E2E_TMP/build.XXXX")"
  cat > "$ctx/Dockerfile"
  ref build -q -t "$name" "$ctx" >/dev/null
  ref save "$name" | pb load >/dev/null
  E2E_BUILT+=("$name")
}
E2E_BUILT=()

# The path of the object (in podbag's store) of the file at `path` in image `name`: from
# the layer indexes, as the store's owner reads them.
object_of() {
  as_owner python3 - "$PODBAG_DATA_DIR/store" "$1" "$2" <<'EOF'
import json, os, sys
store, name, path = sys.argv[1:]
refs = json.load(open(f"{store}/refs.json"))
digest = next(v["digest"] for k, v in refs.items() if k.endswith(name))
record = json.load(open(f"{store}/images/{digest.split(':')[1]}/image.json"))
key = None
for diff_id in record["diff_ids"]:
    index = json.load(open(f"{store}/layers/{diff_id.split(':')[1]}.json"))
    for e in index["entries"]:
        if e["path"] == path and isinstance(e["kind"], dict) and "File" in e["kind"]:
            key = e["kind"]["File"]["object"]
if key is None:
    sys.exit(f"{path} is no file in {name}")
print(f"{store}/objects/{key[:2]}/{key}")
EOF
}

cleanup() {
  local ids m
  ids="$(pb ps -aq 2>/dev/null || true)"
  # shellcheck disable=SC2086
  [ -z "$ids" ] || pb rm -f $ids >/dev/null 2>&1 || true
  as_owner pkill -TERM -f "__keeper $PODBAG_RUNTIME_DIR\$" 2>/dev/null || true
  # Root mode's image mounts (composefs) stay until their image goes: take them down.
  # shellcheck disable=SC2013 # mount points without spaces (a mktemp directory)
  for m in $(awk -v d="$E2E_TMP" 'index($2, d) == 1 { print $2 }' /proc/mounts | sort -r); do
    sudo -n umount -l "$m" 2>/dev/null || true
  done
  # The reference engine's containers of these tests are named podbag-e2e-*.
  for m in $(ref ps -aq --filter name=podbag-e2e- 2>/dev/null); do
    ref rm -f "$m" >/dev/null 2>&1 || true
  done
  for m in "${E2E_BUILT[@]}"; do ref rmi -f "$m" >/dev/null 2>&1 || true; done
  # Rootless, image files belong to subordinate IDs: podbag removes them.
  pb system prune -f >/dev/null 2>&1 || true
  as_owner rm -rf "$E2E_TMP" 2>/dev/null || true
}
trap cleanup EXIT
