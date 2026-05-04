#!/usr/bin/env bash
# Probe a freshly attached device, classify the prompt, decide what kind of
# environment we're talking to. Output: a single line "kind: <classification>".
#
# Useful as the first thing an agent runs after attaching to an unknown board.
set -euo pipefail

SOCK=${TETHER_SOCK:-/tmp/tetherd.sock}

PROMPT=$(tether -s "$SOCK" sync --idle-ms 500 --timeout-ms 3000)
echo "prompt: $(printf %q "$PROMPT")"

case "$PROMPT" in
    *"=> ")
        echo "kind: bootloader (likely U-Boot)"
        ;;
    *"# ")
        echo "kind: root shell (Linux/busybox as root)"
        ;;
    *"$ ")
        echo "kind: user shell (Linux/busybox unprivileged)"
        ;;
    *">"|*">>"|*"]>")
        echo "kind: monitor / REPL (vendor or MCU)"
        ;;
    *":")
        echo "kind: login prompt — credentials required"
        ;;
    "")
        echo "kind: unknown — no prompt detected (try sending '?' or '\\r')"
        ;;
    *)
        echo "kind: unknown shell — got prompt, but didn't match a known pattern"
        ;;
esac
