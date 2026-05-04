#!/usr/bin/env bash
# Run `help`, extract the list of available commands, and decide what to do
# next based on which commands the device exposes.
#
# This is a useful pattern when an agent encounters an unfamiliar shell:
# "what can I run here?".
#
# Usage:
#   bash examples/parse-help.sh                # prints command list
#   bash examples/parse-help.sh has reset      # exit 0 if `reset` is supported
set -euo pipefail

SOCK=${TETHER_SOCK:-/tmp/tetherd.sock}

PROMPT=$(tether -s "$SOCK" sync --idle-ms 500 --timeout-ms 3000)
[[ -z "$PROMPT" ]] && { echo >&2 "no prompt detected"; exit 1; }

# `help` output on most embedded shells looks like:
#     bdinfo    - print Board Info structure
#     boot      - boot default, i.e., run 'bootcmd'
#     ...
# So we extract the first whitespace-delimited token of every line that
# starts with a non-space and contains "  - " (two-space dash).
HELP_OUTPUT=$(
    tether -s "$SOCK" --json run "help" \
        --newline crlf -u "$PROMPT" --literal \
        --timeout-ms 5000 --max-output-bytes 65536 \
        | jq -r '.output'
)

extract_commands() {
    echo "$HELP_OUTPUT" \
        | grep -E '^[[:space:]]*[a-zA-Z][a-zA-Z0-9_-]*[[:space:]]+- ' \
        | awk '{ print $1 }' \
        | sort -u
}

cmd="${1:-list}"
case "$cmd" in
    list)
        extract_commands
        ;;
    has)
        target="${2:?usage: $0 has <command>}"
        if extract_commands | grep -qx "$target"; then
            echo "yes"; exit 0
        else
            echo "no"; exit 1
        fi
        ;;
    *)
        echo >&2 "usage: $0 [list | has <command>]"
        exit 2
        ;;
esac
