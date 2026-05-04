#!/usr/bin/env bash
# Issue `reset` on the device, wait for it to come back to its prompt.
# Useful after flashing firmware, changing env vars that need a reboot, etc.
#
# Strategy: send the reset command, then poll for a known prompt with
# generous timeout. The device disappears for several seconds during reboot;
# we keep polling until either the prompt comes back or we hit the deadline.
#
# Usage:
#   bash examples/wait-for-boot.sh                          # default 60s wait
#   bash examples/wait-for-boot.sh 120                      # 120s wait
#   RESET_CMD=reboot bash examples/wait-for-boot.sh         # use `reboot` instead
set -euo pipefail

SOCK=${TETHER_SOCK:-/tmp/tetherd.sock}
DEADLINE_S="${1:-60}"
RESET_CMD="${RESET_CMD:-reset}"

# Detect the prompt before reboot so we know what to wait for.
PROMPT=$(tether -s "$SOCK" sync --idle-ms 500 --timeout-ms 3000)
[[ -z "$PROMPT" ]] && { echo >&2 "no prompt detected"; exit 1; }
echo "pre-reset prompt: $(printf %q "$PROMPT")"

# Issue the reset. Don't expect a response — the device will start rebooting.
echo "issuing $RESET_CMD …"
tether -s "$SOCK" send "$RESET_CMD" --newline crlf >/dev/null

# Poll for the prompt to come back. We send a CR every couple of seconds
# (most bootloaders re-print the prompt on CR), and check whether we get
# anything matching back within a short window.
echo "waiting up to ${DEADLINE_S}s for prompt to return…"
START=$(date +%s)
while :; do
    NOW=$(date +%s)
    ELAPSED=$(( NOW - START ))
    if (( ELAPSED >= DEADLINE_S )); then
        echo "timeout: prompt did not return within ${DEADLINE_S}s"
        exit 124
    fi
    # Try a quick run with the original prompt as the anchor.
    if tether -s "$SOCK" --json run "" \
        --newline crlf -u "$PROMPT" --literal \
        --timeout-ms 2000 >/dev/null 2>&1
    then
        echo "back up after ${ELAPSED}s"
        exit 0
    fi
    sleep 1
done
