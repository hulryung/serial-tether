# Serial Tether — Guide for AI Agents

> One page. Read once. You can drive an embedded serial console after this.
>
> **Want the *why* before the *how*?** Read [OVERVIEW.md](OVERVIEW.md) first
> for the mental model and architecture. This file is a script-level cookbook.

You are an AI agent that has been given access to `tether`, a CLI for talking
to a serial device through the `tetherd` daemon. The daemon owns the port;
you attach either over a Unix socket (local) or directly via TCP with a token
(remote — see "Remote daemons" below).

## Single canonical command

```sh
tether --json run "<COMMAND>" --newline crlf -u "<PROMPT_REGEX>" --timeout-ms <T>
```

This is the form you should reach for in 95% of cases. It:

- Sends `<COMMAND>` plus `\r\n` over the wire (most embedded shells expect that).
- Holds a writer lock at the daemon, so other clients can't interleave bytes.
- Waits for `<PROMPT_REGEX>` to appear, with a hard timeout.
- Returns a JSON object with the decoded response.

`--newline lf` if the device wants Unix line endings. `--newline none` (default)
if the command already contains the newline you want.

Add `--literal` to treat `<PROMPT_REGEX>` as a fixed string instead of a regex.

## Stable JSON fields

```jsonc
{
  "matched":      true,                  // bool — whether the prompt was found
  "match":        "ASAD SOC => ",        // the matched substring
  "output":       "...response text...", // UTF-8 decoded; ANSI/echo already stripped
  "duration_ms":  404,                   // how long the run took
  "truncated":    false,                 // true if `output` was capped
  "original_bytes": 4622,                // pre-truncation length (when truncated)
  "match_seq":    12450,                 // ring-buffer offset (debug only)
  "end_seq":      12462,
  "before":       "..."                  // base64 raw — prefer `output` instead
}
```

**Use `output` for everything LLM-related.** It is UTF-8 text with ANSI escapes
removed and the echoed command line stripped from the front. `before` is the
raw base64 source — only use it when you need exact bytes (binary protocols,
non-UTF-8 boards).

## Exit codes

```
 0   ok / matched
 2   protocol/RPC error             (other JSON-RPC failures)
 3   connection error               (couldn't dial the socket — daemon down?)
 4   device disconnected            (USB unplugged, etc.)
 5   buffer overflow                (--max-bytes hit without a match)
 6   lock contention                (--preempt=fail and someone else holds the lock)
 7   unauthorized                   (TCP transport: --auth-token missing or wrong)
 124 timeout                        (no match within --timeout-ms)
```

Branch on these in shell scripts:

```sh
if tether --json run "ping" -u "$PROMPT" --newline crlf --timeout-ms 5000 > /tmp/r.json; then
  echo "ok"; cat /tmp/r.json | jq .output
else
  case $? in
    124) echo "timeout — device alive but unresponsive";;
    4)   echo "device gone — abort";;
    *)   echo "other failure — see stderr";;
  esac
fi
```

Stderr always carries a one-line context message on failure (timeout, etc.).

## First-attach procedure

When you first connect to an unfamiliar device, do this exact dance:

```sh
# 1. Confirm the daemon is alive and pointing at the right device.
tether --json status | jq '{device: .device.path, baud: .device.baud, head: .buffer.head_seq}'

# 2. Detect the prompt. Sends a CR, waits for idle, returns the last non-empty line.
PROMPT=$(tether sync --idle-ms 500 --timeout-ms 3000)
echo "prompt: $PROMPT"

# 3. Use that exact string as the --until target (with --literal, since prompts
#    often contain regex metacharacters like '.', '$', '>').
tether --json run "version" --newline crlf -u "$PROMPT" --literal --timeout-ms 3000
```

Don't guess the prompt. Boards differ. `sync` is cheap.

## Race-free patterns

- **Always prefer `run` over `send` + `expect`.** `run` is a single transaction
  that holds the writer lock, captures the seq before the write, and matches
  starting from there. There is no race.
- **Don't compose `send` + `expect` yourself unless you have a reason.** When
  multiple clients are attached (human + agent), interleaving bytes from
  separate sends can corrupt either client's matching.
- **Set `--timeout-ms` explicitly every time.** Default is 3000ms. For boot,
  flash, or memory dumps, raise it. Never pass `null`/0 expecting "wait
  forever" — use a generous concrete bound like `60000`.

## Length safety

- `--max-output-bytes 8192` is the default. If `truncated: true` you got the
  *trailing* N bytes (since the prompt is at the end, you usually want this).
- Set lower (e.g. `1024`) when you only care about pass/fail.
- For commands that print megabytes (`md`, `ext4ls`, `cat /var/log/...`),
  consider piping through paging or grep on the device side rather than
  letting the host swallow it.

## Common pitfalls

| Symptom | Cause | Fix |
|---|---|---|
| Timeout, no match | Forgot `--newline` | Add `--newline crlf` (or `lf`) |
| Output begins with the command itself | `--strip-echo` got disabled | Re-enable (it's on by default) |
| Match found in *previous* output | Used `expect` without anchoring | Use `run` instead |
| Garbled / random bytes | Wrong baud rate | `tether status` shows the configured baud |
| `output` empty, exit 0 | Pattern matched at offset 0 (empty before) | Add anchor like `^` or use `--literal` with full prompt |
| Connection error | Daemon not running, or wrong `-s` socket | Check `ls /tmp/tetherd.sock` |

## What NOT to do

- **Don't use stdin streaming / interactive mode.** That's for humans. Every
  command should be one `run` call with explicit pattern and timeout.
- **Don't omit `--timeout-ms`.** A wrong default in your script will hang the
  agent loop for 3 seconds × N retries.
- **Don't strip the prompt manually from `output`.** It's already separated.
  The prompt is in `match`, the response is in `output`.
- **Don't parse `--version` or human-readable output.** Always use `--json`.
- **Don't open multiple writer-lock sessions in parallel.** If you need
  concurrent reads (tail), do `tether tail` in one connection and `tether run`
  in another. Don't try to send commands from two parallel `run`s racing.

## Detecting what kind of device you're talking to

```sh
PROMPT=$(tether sync --idle-ms 500)
case "$PROMPT" in
  *"=> ")            echo "U-Boot or similar bootloader";;
  *"# ")             echo "root shell (busybox/Linux as root)";;
  *"$ ")             echo "user shell";;
  *">"|*">>"|*"]>")  echo "MCU REPL or vendor monitor";;
  *)                 echo "unknown — try 'help' or '?' or '\\r'";;
esac
```

This is a starting heuristic, not gospel. Verify with `version`, `uname`, or
whatever the board accepts.

## Remote daemons (over a network)

Native TCP transport with token auth (v0.4.0+; v0.5 lets you say just `--tcp`):

```sh
# Daemon host (board operator):
tetherd -D /dev/tty.usbserial-XXXX --tcp --auth-token MYSECRET
# (bare `--tcp` defaults to 0.0.0.0:5557; omit --auth-token to auto-generate)

# Agent host (you):
export TETHER_AUTH_TOKEN=MYSECRET
tether -s tcp://daemon-host:5557 status
tether -s tcp://daemon-host:5557 run "version" --newline crlf -u "# " --literal
```

UDS still works for local connections (OS-level auth via file permissions).
A single daemon can listen on both transports at once.

If you're stuck with a pre-v0.4 daemon you cannot upgrade, fall back to SSH
forwarding the UDS:

```sh
# Forward the remote socket onto your local filesystem (one-time per session).
ssh -N -L /tmp/tetherd-remote.sock:/tmp/tetherd.sock user@daemon-host &
tether -s /tmp/tetherd-remote.sock status
```

Don't waste effort on socat bridges; native TCP is simpler and the wire
protocol is identical.

## Worked example

Goal: read U-Boot environment, set a variable, save it, verify.

```sh
#!/usr/bin/env bash
set -e
PROMPT=$(tether sync --idle-ms 500 --timeout-ms 3000)

run() {
  tether --json run "$1" --newline crlf -u "$PROMPT" --literal --timeout-ms 5000 \
    | jq -r .output
}

echo "--- before ---"
run "printenv bootdelay"

run "setenv bootdelay 5"
run "saveenv"

echo "--- after ---"
run "printenv bootdelay"
```

That's the shape of most board automation. `sync` once at the start, `run` per
command with the same prompt, parse `output`.

## Reference cheatsheet

```
tether status                                                # daemon + device info
tether sync --idle-ms 500                                    # detect prompt
tether tail                                                  # stream output (read-only follow)
tether send "any-bytes" --newline crlf                       # fire-and-forget
tether run "<cmd>" -u "<prompt>" --literal --newline crlf    # canonical
tether expect "<regex>" --timeout-ms <T>                     # rare; prefer run
```

That's it. You're ready.
