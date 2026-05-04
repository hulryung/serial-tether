#!/usr/bin/env bash
# Read a few diagnostic commands on a U-Boot device and emit a single JSON
# document combining the results.
#
# Usage:
#   bash examples/uboot-bootinfo.sh
#   bash examples/uboot-bootinfo.sh | jq .
set -euo pipefail

SOCK=${TETHER_SOCK:-/tmp/tetherd.sock}

PROMPT=$(tether -s "$SOCK" sync --idle-ms 500 --timeout-ms 3000)
[[ -z "$PROMPT" ]] && { echo >&2 "no prompt detected"; exit 1; }

# Run a command and return its `output` field (decoded text).
run_cmd() {
    local cmd="$1"
    tether -s "$SOCK" --json run "$cmd" \
        --newline crlf -u "$PROMPT" --literal \
        --timeout-ms 5000 --max-output-bytes 16384 \
        | python3 -c 'import sys,json; print(json.load(sys.stdin)["output"])'
}

VERSION=$(run_cmd "version")
BDINFO=$(run_cmd "bdinfo")
ENV=$(run_cmd "printenv")

jq -n \
    --arg prompt  "$PROMPT" \
    --arg version "${VERSION%$'\n'}" \
    --arg bdinfo  "${BDINFO%$'\n'}" \
    --arg env     "${ENV%$'\n'}" \
    '{prompt: $prompt, version: $version, bdinfo: $bdinfo, env: $env}'
