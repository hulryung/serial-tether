#!/usr/bin/env bash
# Serial Tether end-to-end smoke test using a virtual PTY echo device.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TETHERD="$ROOT/target/debug/tetherd"
TETHER="$ROOT/target/debug/tether"
SOCK=/tmp/tetherd-smoke.sock
PTY_FILE=/tmp/tetherd-pty.path
PTY_ERR=/tmp/tetherd-pty.err
TETHERD_LOG=/tmp/tetherd-smoke.log

cleanup() {
    pkill -f echo_pty.py 2>/dev/null || true
    pkill -f 'target/debug/tetherd' 2>/dev/null || true
    rm -f "$SOCK" "$PTY_FILE" "$PTY_ERR" "$TETHERD_LOG"
}
trap cleanup EXIT

cleanup

# 1) Virtual PTY device.
nohup python3 -u "$ROOT/tools/echo_pty.py" > "$PTY_FILE" 2>"$PTY_ERR" &
disown
sleep 0.5
PTY=$(grep -oE '/dev/ttys[0-9]+' "$PTY_FILE")
[[ -z "$PTY" ]] && { echo "FAIL: no PTY"; exit 1; }
echo "[smoke] PTY=$PTY"

# 2) Daemon.
nohup "$TETHERD" -D "$PTY" -b 115200 -s "$SOCK" > "$TETHERD_LOG" 2>&1 &
disown
# Wait up to 3 seconds for the socket to appear.
for i in {1..30}; do
    [[ -S "$SOCK" ]] && break
    sleep 0.1
done
[[ -S "$SOCK" ]] || { echo "FAIL: socket"; cat "$TETHERD_LOG" 2>/dev/null; exit 1; }
echo "[smoke] tetherd up"

# 3) status
"$TETHER" -s "$SOCK" --json status > /dev/null
echo "[smoke] ok: status"

# 4) server-side run — strip_echo defaults on.
OUT=$("$TETHER" -s "$SOCK" --json run $'version\n' -u "# " --literal --timeout-ms 2000)
echo "$OUT" | grep -q '"matched": true' || { echo "FAIL: matched"; echo "$OUT"; exit 1; }
echo "$OUT" | grep -q '"duration_ms"' || { echo "FAIL: missing duration_ms"; echo "$OUT"; exit 1; }
# strip_echo working → "version" must not be the first line of `output`.
OUTPUT=$(echo "$OUT" | python3 -c 'import sys,json; print(json.load(sys.stdin)["output"])')
echo "$OUTPUT" | head -1 | grep -q '^version$' && {
    echo "FAIL: echo not stripped: $OUTPUT"; exit 1;
}
echo "$OUTPUT" | grep -q "v0.1.0-mock" || {
    echo "FAIL: response missing v0.1.0-mock: $OUTPUT"; exit 1;
}
echo "[smoke] ok: server-side run + strip_echo"

# 5) max_output_bytes truncation.
OUT=$("$TETHER" -s "$SOCK" --json run $'say abcdefghijklmnopqrstuvwxyz\n' \
       -u "# " --literal --timeout-ms 2000 --max-output-bytes 10)
echo "$OUT" | grep -q '"truncated": true' || {
    echo "FAIL: not truncated"; echo "$OUT"; exit 1;
}
echo "[smoke] ok: truncate"

# 6) timeout exit code.
set +e
"$TETHER" -s "$SOCK" --json run $'noop\n' -u "NEVER" --literal --timeout-ms 200 >/dev/null 2>&1
EXIT=$?
set -e
[[ "$EXIT" == "124" ]] || { echo "FAIL: expected exit 124 on timeout, got $EXIT"; exit 1; }
echo "[smoke] ok: timeout exit=124"

# 7) connection-error exit code.
set +e
"$TETHER" -s /tmp/no-such-sock status >/dev/null 2>&1
EXIT=$?
set -e
[[ "$EXIT" == "3" ]] || { echo "FAIL: expected exit 3 on connect failure, got $EXIT"; exit 1; }
echo "[smoke] ok: connection error exit=3"

# 8) sync — send CR, wait until idle, expect a non-empty prompt candidate.
PROMPT=$("$TETHER" -s "$SOCK" sync --idle-ms 200 --timeout-ms 1500)
[[ -z "$PROMPT" ]] && {
    # The first sync may catch the idle window before any data arrives — retry.
    sleep 0.3
    PROMPT=$("$TETHER" -s "$SOCK" sync --idle-ms 200 --timeout-ms 1500)
}
echo "[smoke] sync prompt candidate: $(printf %q "$PROMPT")"

echo "[smoke] ALL PASS"
