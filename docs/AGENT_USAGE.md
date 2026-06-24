# Serial Tether — Guide for AI Agents

> One page. Read once. You can drive an embedded serial console after this.
> The same cookbook is available offline straight from the CLI: **`tether agents`**.
>
> **Want the *why* before the *how*?** Read [OVERVIEW.md](OVERVIEW.md) first
> for the mental model and architecture. This file is a script-level cookbook.

You are an AI agent that has been given access to `tether`, a CLI for talking
to a serial device through the `tetherd` daemon. The daemon owns the port;
you attach either over a Unix socket (local) or directly via TCP with a token
(remote — see "Remote daemons" below).

## Rules — read these first

1. **Target the device explicitly with `-d <id>`.** Multi-device daemons
   require it (they answer `-32015 ambiguous_device` otherwise). Discover ids
   with `tether list-devices --json`. Single-device daemons let you omit it.
2. **Stay non-interactive: `export TETHER_NONINTERACTIVE=1`** (or pass
   `--no-interactive`). When a device/port is ambiguous *and* tether detects a
   terminal, it may show an interactive picker — which would hang an agent that
   runs under a PTY. With this set it fails fast with the normal error instead.
   Plain pipes/scripts are already non-interactive.
3. **Always `--json` for scripted calls.** Never parse human-readable output.
4. **Prefer `exec` for shell commands, `run` for raw consoles** (next section).

## Two canonical commands

### Device at a POSIX shell prompt → `exec`

Busybox / Linux / dash / U-Boot hush sitting at a prompt:

```sh
tether -d <id> exec "<command>"            # stdout = the command's output
tether -d <id> exec "<command>" --json     # {output, exit_code, duration_ms}
```

`exec` wraps the command so the device shell brackets its output with unique
markers, returns only the bytes the command produced (echoed command stripped,
even when the terminal wraps a long line), and **exits with the device
command's own status, like `ssh`**:

```sh
if tether -d board0 exec "test -f /etc/os-release"; then echo present; fi
```

No prompt to detect, no `--until` to craft. This is the 90% case once a shell
is up. Default line ending is `cr`; pass `--newline lf`/`crlf` if the device
needs it.

### Raw / non-shell console → `run`

Bootloader mid-boot, login prompt, vendor MCU monitor — anything without a
shell to run `echo`/`$?`:

```sh
tether -d <id> --json run "<COMMAND>" --newline crlf -u "<PROMPT_REGEX>" --timeout-ms <T>
```

`run` is a single daemon-side transaction: it holds the writer lock, sends
`<COMMAND>` plus the newline you choose, and waits for `<PROMPT_REGEX>` with a
hard timeout — race-free, no interleaving from other clients. `--newline lf`
for Unix endings, `--newline none` (default) if the command already contains
its terminator. Add `--literal` to treat the pattern as a fixed string instead
of a regex (prompts often contain `.`, `$`, `>`).

## Stable JSON fields

`exec --json`:

```jsonc
{ "output": "...", "exit_code": 0, "duration_ms": 12, "truncated": false }
```

`run --json` / `expect --json`:

```jsonc
{
  "matched":        true,                  // bool — whether the prompt was found
  "match":          "ASAD SOC => ",        // the matched substring
  "output":         "...response text...", // UTF-8 decoded; ANSI/echo already stripped
  "duration_ms":    404,                   // how long the run took
  "truncated":      false,                 // true if `output` was capped
  "original_bytes": 4622,                  // pre-truncation length (when truncated)
  "match_seq":      12450,                 // ring-buffer offset (debug only)
  "end_seq":        12462,
  "before":         "..."                  // base64 raw — prefer `output` instead
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

`exec` additionally **passes through the device command's own exit status**
(like ssh) — so a non-zero exit from `exec` usually means your *device* command
failed, not tether. Branch on it directly:

```sh
if out=$(tether -d board0 exec "cat /proc/uptime" --json); then
  echo "$out" | jq -r .output
else
  case $? in
    124) echo "timeout — device alive but unresponsive";;
    4)   echo "device gone — abort";;
    *)   echo "device command failed (rc=$?)";;
  esac
fi
```

Stderr always carries a one-line context message on failure (timeout, etc.).

## First-attach procedure

When you first connect to an unfamiliar setup, do this exact dance:

```sh
# 1. What's connected? ids, paths, connected state.
tether --json list-devices

# 2. Confirm the device + baud for the one you want.
tether -d <id> --json status | jq '{path: .device.path, baud: .device.baud}'

# 3. Is it at a shell? Just try exec.
tether -d <id> exec "echo READY && id"

# If exec times out or the device is a raw console, detect the prompt and use run:
PROMPT=$(tether -d <id> sync --idle-ms 500 --timeout-ms 3000)
tether -d <id> --json run "version" --newline crlf -u "$PROMPT" --literal --timeout-ms 3000
```

Don't guess the prompt for `run`. Boards differ. `sync` is cheap.

## Race-free patterns

- **For shells use `exec`; for raw consoles use `run`.** Both are single
  transactions — `run` holds the writer lock, captures the seq before the write,
  and matches from there; `exec` is built on the same primitive.
- **Don't hand-compose `send` + `expect` unless you have a reason.** When
  multiple clients are attached (human + agent), interleaving bytes from
  separate sends can corrupt either client's matching.
- **Set `--timeout-ms` explicitly every time.** Default is 3000ms (`exec`:
  5000ms). For boot, flash, or memory dumps, raise it. Never pass `null`/0
  expecting "wait forever" — use a generous concrete bound like `60000`.

## Length safety

- Default output cap is `--max-output-bytes 8192` for `run`/`expect`, `65536`
  for `exec`. If `truncated: true` you got the *trailing* N bytes (the prompt /
  end-marker is at the end, so you usually want this).
- Set lower (e.g. `1024`) when you only care about pass/fail.
- For commands that print megabytes (`md`, `ext4ls`, `cat /var/log/...`),
  filter on the device side (`| tail`, `| grep`) rather than letting the host
  swallow it.

## Common pitfalls

| Symptom | Cause | Fix |
|---|---|---|
| `-32015 ambiguous_device` | Multi-device daemon, no `-d` | Add `-d <id>` (`list-devices` to find it) |
| Agent hangs on a menu | PTY + ambiguous target | Set `TETHER_NONINTERACTIVE=1` and pass `-d <id>` |
| `run` times out, no match | Forgot `--newline` | Add `--newline crlf` (or `lf`) |
| `exec` times out | Device isn't at a POSIX shell | Use `run`/`send`/`expect` for raw consoles |
| Output begins with the command itself | `run --strip-echo` disabled | Re-enable (on by default); `exec` strips it always |
| Match found in *previous* output | `expect` without anchoring | Use `run` (or `exec`) instead |
| Garbled / random bytes | Wrong baud rate | `tether -d <id> status` shows the configured baud |
| Connection error | Daemon not running, or wrong `-s` | Check `ls /tmp/tetherd.sock`; start a daemon |

## What NOT to do

- **Don't use stdin streaming / interactive shell mode.** That's for humans.
  Every command should be one `exec` (or `run`) call.
- **Don't omit `-d <id>` on a multi-device daemon.** You'll get `-32015`, or —
  under a PTY without `TETHER_NONINTERACTIVE` — an interactive menu that stalls.
- **Don't omit `--timeout-ms`.** A wrong default will hang your loop.
- **Don't strip the prompt/marker manually from `output`.** It's already
  separated — `match` holds the prompt, `output` the response; `exec` returns
  output only.
- **Don't parse `--version` or human-readable output.** Always use `--json`.
- **Don't open multiple writer-lock sessions in parallel.** For concurrent
  reads, run `tether -d <id> tail` in one connection and `exec`/`run` in
  another. Don't race two parallel `run`/`exec` writes.

## Connecting when a daemon may already be running

The normal case: a long-lived `tetherd` is already up (someone ran it, or a
human has a `tether shell` open). **Just attach as a client** — don't spawn a
competing daemon for the same port.

```sh
# Is anyone home? Either succeeding means a daemon is running.
tether --json list-devices       # ids/paths it manages
tether -d <id> --json status     # the device you care about

# Then drive it as a client (no daemon management on your side):
tether -d <id> exec "<cmd>"
```

Notes for automation:

- **`tether -D <PATH>` / `tether <PATH>` are "lazy spawn".** They probe
  `/tmp/tetherd*.sock` first; if an existing daemon already owns that path they
  attach to it as a client (a short "attaching as a client" notice on stderr)
  instead of starting a second daemon that would scramble the port. You don't
  need a separate `pgrep tetherd` check.
- **Bare `tether` with no daemon will, *in an interactive terminal*, offer a
  port picker and start a throwaway session daemon.** Agents should not rely on
  that: with `TETHER_NONINTERACTIVE=1` there's no prompt, so be explicit — start
  a daemon (`tetherd -D /dev/ttyUSB0 -b 115200 &`) or use the standalone form
  (`tether -D /dev/ttyUSB0 exec "..."`).

## Detecting what kind of device you're talking to

```sh
PROMPT=$(tether -d <id> sync --idle-ms 500)
case "$PROMPT" in
  *"=> ")            echo "U-Boot or similar bootloader";;
  *"# ")             echo "root shell (busybox/Linux as root)";;
  *"$ ")             echo "user shell";;
  *">"|*">>"|*"]>")  echo "MCU REPL or vendor monitor";;
  *)                 echo "unknown — try 'help' or '?' or '\\r'";;
esac
```

A starting heuristic, not gospel. Shells (`#`, `$`) → use `exec`; bootloaders /
monitors (`=>`, `>`) → use `run`. Verify with `version`, `uname`, etc.

## Multiple boards on one host

Two patterns. Prefer **B** (one daemon, many devices) — fewer moving parts and
no port double-open.

**A. One daemon per board** (`--name` selects which daemon):

```sh
tetherd -D /dev/ttyUSB0 --name board0 &
tetherd -D /dev/ttyUSB1 --name board1 &

tether --name board0 status
tether --name board1 exec "uname -a"
```

`--name X` is shorthand for `-s /tmp/tetherd-X.sock`.

**B. One daemon, multiple devices** (`-d` / `--device` selects which
device-in-daemon):

```sh
# Each -D is `[id=]path[,key=value,...]`. Per-device baud / parity / data-bits
# / stop-bits / flow override the global flags.
tetherd \
  -D 'board0=/dev/ttyUSB0' \
  -D 'board1=/dev/ttyUSB1,baud=921600' &

# Address devices by id.
tether -d board0 status
tether -d board0 exec "uname -a"
tether -d board1 sync
tether list-devices         # daemon-wide; no -d needed
```

Multi-device daemons answer `-32015 ambiguous_device` if `-d` is missing and
>1 device is managed. Single-device daemons fall back to the only device, so
`-d` is optional there.

## Tio-style line / break / modem control

```sh
tether -d <id> break --duration-ms 250    # send a BREAK pulse
tether -d <id> dtr on                      # assert DTR
tether -d <id> dtr off                     # deassert
tether -d <id> rts on                      # assert RTS
tether -d <id> lines                       # → CTS=1 DSR=0 RI=0 DCD=1
tether -d <id> disconnect                  # explicit close (parks port)
tether -d <id> connect                     # release the hold, reopen
tether -d <id> config                      # show live baud/framing/flow
tether -d <id> config --baud 921600        # change it live
```

The Fd backend (PTYs, pipes) returns `-32007 unsupported_serial_op` for
break / dtr / rts / lines.

## Remote daemons (over a network)

Native TCP transport with token auth:

```sh
# Daemon host (board operator):
tetherd -D /dev/tty.usbserial-XXXX --tcp --auth-token MYSECRET
# (bare `--tcp` defaults to 0.0.0.0:5557; omit --auth-token to auto-generate)

# Agent host (you):
export TETHER_AUTH_TOKEN=MYSECRET
export TETHER_NONINTERACTIVE=1
tether -s tcp://daemon-host:5557 -d <id> status
tether -s tcp://daemon-host:5557 -d <id> exec "uname -a"
```

UDS still works for local connections (OS-level auth via file permissions). A
single daemon can listen on both transports at once.

If you're stuck with a pre-TCP daemon you cannot upgrade, fall back to SSH
forwarding the UDS:

```sh
ssh -N -L /tmp/tetherd-remote.sock:/tmp/tetherd.sock user@daemon-host &
tether -s /tmp/tetherd-remote.sock -d <id> status
```

Don't waste effort on socat bridges; native TCP is simpler and the wire
protocol is identical.

## Worked example A — Linux shell (use `exec`)

Goal: read a value, act on it, check a file, all by exit code.

```sh
#!/usr/bin/env bash
set -e
D=board0

tether -d $D exec "uname -a"
UP=$(tether -d $D exec "cut -d. -f1 /proc/uptime" --json | jq -r .output)
echo "uptime ${UP}s"

if tether -d $D exec "test -e /dev/mmcblk1"; then
  echo "eMMC present"
else
  echo "no eMMC — exit $?"
fi
```

## Worked example B — U-Boot (raw console, use `run`)

Goal: read U-Boot environment, set a variable, save it, verify.

```sh
#!/usr/bin/env bash
set -e
D=board0
PROMPT=$(tether -d $D sync --idle-ms 500 --timeout-ms 3000)

ub() {
  tether -d $D --json run "$1" --newline crlf -u "$PROMPT" --literal --timeout-ms 5000 \
    | jq -r .output
}

echo "--- before ---"; ub "printenv bootdelay"
ub "setenv bootdelay 5"
ub "saveenv"
echo "--- after ---";  ub "printenv bootdelay"
```

`sync` once at the start, `run` per command with the same prompt, parse `output`.

## Reference cheatsheet

```
tether agents                                               # this guide, offline
tether list-devices --json                                  # ids / paths / connected
tether -d <id> status                                       # daemon + device info
tether -d <id> exec "<cmd>"                                 # shell: output + exit code
tether -d <id> exec "<cmd>" --json                          # {output, exit_code, duration_ms}
tether -d <id> run "<cmd>" -u "<prompt>" --literal --newline crlf   # raw console
tether -d <id> sync --idle-ms 500                           # detect prompt
tether -d <id> tail                                         # stream output (read-only follow)
tether -d <id> send "any-bytes" --newline crlf              # fire-and-forget
tether -d <id> expect "<regex>" --timeout-ms <T>            # rare; prefer run/exec
```

Set `TETHER_NONINTERACTIVE=1` once in your environment. That's it. You're ready.
