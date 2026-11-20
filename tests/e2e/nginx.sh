#!/usr/bin/env bash
# nginx in podbag, end to end: a published port, the page, the access log, stop and
# removal; then the same through the docker CLI on podbag's Docker API. The page and the
# headers nginx makes from the file (ETag and Last-Modified come from its time and size)
# must be the reference engine's.
# shellcheck source=tests/e2e/lib.sh
. "$(dirname "$0")/lib.sh"

IMAGE=mirror.gcr.io/library/nginx:1.27-alpine
PORT_REF=18081
PORT_PB=18082
PORT_API=18083

wait_http() {
  for _ in $(seq 100); do
    curl -fsS -o /dev/null "http://127.0.0.1:$1/" 2>/dev/null && return 0
    sleep 0.1
  done
  return 1
}

# The page's checksum and the headers that come from the file.
page() {
  curl -fsS "http://127.0.0.1:$1/" | sha256sum | cut -d' ' -f1
  curl -fsSI "http://127.0.0.1:$1/" | tr -d '\r' |
    grep -iE '^(etag|last-modified|content-length|content-type):' | sort
}

ref rm -f podbag-e2e-nginx >/dev/null 2>&1 || true
ref run -d --name podbag-e2e-nginx -p "127.0.0.1:$PORT_REF:80" "$IMAGE" >/dev/null
trap 'ref rm -f podbag-e2e-nginx >/dev/null 2>&1 || true; cleanup' EXIT
wait_http "$PORT_REF" || fail "nginx does not answer in the reference engine"
want="$(page "$PORT_REF")"

# --- the podbag CLI -------------------------------------------------------------------

pb pull "$IMAGE" >/dev/null
pb run -d --name web -p "127.0.0.1:$PORT_PB:80" "$IMAGE" >/dev/null
wait_http "$PORT_PB" || fail "nginx does not answer on the published port"
got="$(page "$PORT_PB")"
[ "$got" = "$want" ] || fail "the page differs from the reference engine's:
podbag:
$got
reference:
$want"
pass "nginx serves the page with the reference engine's headers"

for _ in $(seq 50); do
  pb logs web 2>&1 | grep -q '"GET / HTTP/1.1" 200' && break
  sleep 0.1
done
pb logs web 2>&1 | grep -q '"GET / HTTP/1.1" 200' || fail "the access log has no request"
pass "the access log shows the requests"

pb stop web >/dev/null
curl -fsS -o /dev/null --max-time 2 "http://127.0.0.1:$PORT_PB/" 2>/dev/null &&
  fail "the port still answers after stop"
pb rm web >/dev/null || fail "cannot remove the stopped container"
[ -z "$(pb ps -aq)" ] || fail "a container is left: $(pb ps -a)"
pass "stop and rm leave nothing"

# --- the docker CLI on podbag's Docker API -------------------------------------------

if ! command -v docker >/dev/null 2>&1; then
  echo "skip  [$(mode)] the docker CLI is not installed"
  exit 0
fi
sock="$E2E_TMP/docker.sock"
# Not through pb(): $! must be the service (or the sudo that relays signals to it), and
# its output goes to a file, so no one waits for it to close ours.
if [ "$E2E_ROOT" = 1 ]; then
  # shellcheck disable=SC2024 # the log is the test user's
  sudo -n env PODBAG_DATA_DIR="$PODBAG_DATA_DIR" PODBAG_RUNTIME_DIR="$PODBAG_RUNTIME_DIR" \
    PODBAG_CONFIG_DIR="$PODBAG_CONFIG_DIR" "$PODBAG" system service --socket "$sock" --timeout 0 \
    > "$E2E_TMP/service.log" 2>&1 &
else
  "$PODBAG" system service --socket "$sock" --timeout 0 > "$E2E_TMP/service.log" 2>&1 &
fi
service=$!
stop_service() { as_owner kill "$service" 2>/dev/null || true; }
trap 'stop_service; ref rm -f podbag-e2e-nginx >/dev/null 2>&1 || true; cleanup' EXIT
for _ in $(seq 100); do [ -S "$sock" ] && break; sleep 0.1; done
[ -S "$sock" ] || fail "podbag system service did not open its socket: $(cat "$E2E_TMP/service.log")"
dk() {
  if [ "$E2E_ROOT" = 1 ]; then sudo -n env DOCKER_HOST="unix://$sock" docker "$@"; else DOCKER_HOST="unix://$sock" docker "$@"; fi
}
dk run -d --name api-web -p "127.0.0.1:$PORT_API:80" "$IMAGE" >/dev/null
wait_http "$PORT_API" || fail "nginx started by the docker CLI does not answer"
[ "$(page "$PORT_API")" = "$want" ] || fail "the page through the Docker API differs"
dk rm -f api-web >/dev/null
[ -z "$(pb ps -aq)" ] || fail "docker rm -f left a container: $(pb ps -a)"
stop_service
wait "$service" 2>/dev/null || true
pass "the docker CLI runs nginx through podbag's Docker API"
