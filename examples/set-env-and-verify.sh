#!/usr/bin/env bash
# Set a U-Boot env var, persist it with `saveenv`, verify the new value matches.
# Exits 0 on verified, non-zero otherwise.
#
# Usage:
#   bash examples/set-env-and-verify.sh bootdelay 5
#   bash examples/set-env-and-verify.sh ipaddr 192.168.1.42
set -euo pipefail

SOCK=${TETHER_SOCK:-/tmp/tetherd.sock}

NAME="${1:?usage: $0 <name> <value>}"
VALUE="${2:?usage: $0 <name> <value>}"

PROMPT=$(tether -s "$SOCK" sync --idle-ms 500 --timeout-ms 3000)
[[ -z "$PROMPT" ]] && { echo >&2 "no prompt detected"; exit 1; }

run_cmd() {
    tether -s "$SOCK" --json run "$1" \
        --newline crlf -u "$PROMPT" --literal \
        --timeout-ms 5000 \
        | jq -r '.output'
}

echo "[1/3] before:"
BEFORE=$(run_cmd "printenv $NAME" || true)
echo "$BEFORE"

echo "[2/3] setenv + saveenv:"
run_cmd "setenv $NAME '$VALUE'"
run_cmd "saveenv"

echo "[3/3] after:"
AFTER=$(run_cmd "printenv $NAME")
echo "$AFTER"

# Verify: the new value must appear on the printenv line.
if echo "$AFTER" | grep -qE "^$NAME=$(printf %q "$VALUE")$"; then
    echo "OK: $NAME=$VALUE persisted"
    exit 0
else
    echo "FAIL: expected '$NAME=$VALUE' but got:"
    echo "$AFTER"
    exit 1
fi
