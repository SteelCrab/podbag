#!/usr/bin/env bash
# Image files as programs see them, against the reference engine: Python's .pyc files
# stay valid (their sources keep their times), and cron runs the crontabs of the image
# (cron skips a crontab that has more than one link).
# shellcheck source=tests/e2e/lib.sh
. "$(dirname "$0")/lib.sh"

# --- Python ---------------------------------------------------------------------------

build_and_load localhost/podbag-e2e-pyc:1 <<'EOF'
FROM mirror.gcr.io/library/python:3.12-alpine
RUN python3 -m compileall -q /usr/local/lib/python3.12 >/dev/null; true
EOF

# Prints how many .pyc files Python wrote while importing some modules (0 when the
# image's .pyc files match the times of their sources).
pyc_script='
touch /tmp/mark
python3 -c "import json, asyncio, email.mime.text, http.client, logging, argparse"
find /usr/local/lib/python3.12 -name "*.pyc" -newer /tmp/mark | wc -l
'
want="$(ref run --rm localhost/podbag-e2e-pyc:1 sh -c "$pyc_script")"
[ "$want" = 0 ] || fail "the reference engine rewrote $want .pyc files: the test image is wrong"
got="$(pb run --rm --network none localhost/podbag-e2e-pyc:1 sh -c "$pyc_script")"
[ "$got" = 0 ] || fail "python rewrote $got .pyc files: source times were lost"
pass "python keeps the image's .pyc files"

times_script='stat -c "%Y %n" /usr/local/lib/python3.12/json/__init__.py /usr/local/lib/python3.12/json/decoder.py'
[ "$(pb run --rm --network none localhost/podbag-e2e-pyc:1 sh -c "$times_script")" = \
  "$(ref run --rm localhost/podbag-e2e-pyc:1 sh -c "$times_script")" ] ||
  fail "file times differ from the reference engine's"
pass "file times match the reference engine's"

# --- cron -----------------------------------------------------------------------------

# pam_loginuid fails in containers run as root, by any engine: it is turned off.
build_and_load localhost/podbag-e2e-cron:1 <<'EOF'
FROM mirror.gcr.io/library/debian:bookworm-slim
RUN apt-get update -qq \
 && apt-get install -y -qq --no-install-recommends cron >/dev/null \
 && rm -rf /var/lib/apt/lists/* \
 && sed -i '/pam_loginuid/s/^/#/' /etc/pam.d/cron \
 && echo '* * * * * root echo cron.d >> /tmp/ran' > /etc/cron.d/e2e \
 && chmod 644 /etc/cron.d/e2e \
 && echo '* * * * * root echo crontab >> /tmp/ran' >> /etc/crontab \
 && echo '* * * * * echo user >> /tmp/ran' | crontab -
EOF

links="$(pb run --rm --network none localhost/podbag-e2e-cron:1 \
  stat -c %h /etc/crontab /etc/cron.d/e2e /var/spool/cron/crontabs/root | tr '\n' ' ')"
[ "$links" = "1 1 1 " ] || fail "crontabs have more than one link: $links"
pass "crontabs have one link"

# Runs cron until each of the three jobs ran once (the next minute starts them all).
# shellcheck disable=SC2016 # expanded in the container
cron_script='
cron -L 0
for i in $(seq 75); do
  [ "$(sort -u /tmp/ran 2>/dev/null | wc -l)" -ge 3 ] && break
  sleep 1
done
sort -u /tmp/ran 2>/dev/null | tr "\n" " "
'
want="cron.d crontab user "
ref run --rm --name podbag-e2e-cron localhost/podbag-e2e-cron:1 sh -c "$cron_script" \
  > "$E2E_TMP/ref-cron" &
ref_run=$!
got="$(pb run --rm --network none localhost/podbag-e2e-cron:1 sh -c "$cron_script")"
wait "$ref_run" || fail "the reference engine could not run the cron image"
[ "$(cat "$E2E_TMP/ref-cron")" = "$want" ] ||
  fail "cron jobs did not all run with the reference engine: the test image is wrong"
[ "$got" = "$want" ] || fail "cron ran only: '$got' (want '$want')"
pass "cron runs the image's crontabs"
