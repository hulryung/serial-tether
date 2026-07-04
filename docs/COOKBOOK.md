# Serial Tether — Cookbook

Task-oriented recipes for `tether` / `tetherd`, ordered easy → advanced. This
assumes you've read [`GETTING_STARTED.md`](GETTING_STARTED.md) and have a
board talking to *something* — now you want to actually get work done.

Conventions used below: `a35` is the device id (`-d a35`), `/dev/cu.usbserial-XXXX`
is the macOS path, `/dev/ttyUSB0` is the Linux path. Swap in your own.
Every command block is copy-pasteable as written (just substitute the path/id).

---

## Everyday

### 1. Watch a boot log and save it to a clean file

**Goal:** capture everything a board prints while it boots, into a plain text
file, with nothing else mixed in.

```sh
# One-shot daemon + follow, buffered output included:
tetherd -D a35=/dev/ttyUSB0 -b 115200 &
tether -d a35 tail --from start > boot.log
# power-cycle the board, Ctrl-C tail when you have enough
```

Split data from events into two files:

```sh
tether -d a35 tail --from start > boot.log 2> tail-events.log
```

**What's happening:** `tail` is read-only — it doesn't take the writer lock,
so it runs alongside anyone else's `exec`/`run`/`shell`. Device bytes go to
stdout; connect/disconnect notifications go to stderr, so redirecting stdout
alone gives you a clean log with no `[reconnect]`-style noise mixed in.
`--from start` replays whatever is still sitting in the daemon's 64 KiB ring
buffer *before* following live — useful when the board already booted by the
time you thought to start `tail`. `--from now` (the default) skips the replay
and only shows new output.

### 2. Two terminals on one board: shell in one, tail in the other

**Goal:** interact with the board yourself while a second window shows a full
transcript — without either one stepping on the other.

```sh
# Terminal A — you, driving:
tether -d a35 shell             # Ctrl-A then Q to quit

# Terminal B — full transcript:
tether -d a35 tail | tee session.log
```

**What's happening:** every attached session gets its own cursor into the
daemon's ring buffer (in fact *two* cursors — one for `data` notifications,
one for `run`/`expect` matching — see `docs/OVERVIEW.md`). `shell` and `tail`
don't compete for the same bytes; both see everything the device emits, at
whatever pace each side can keep up. This is why a third terminal running
`tether run` or `tether exec` doesn't disturb either of the first two — it
just briefly holds the writer lock for its own transaction.

### 3. Run device commands from a shell script

**Goal:** drive the board from Bash, branch on success/failure, and pull a
single field out of JSON output.

```sh
#!/usr/bin/env bash
set -e
D=a35

tether -d $D exec "uname -a"

if tether -d $D exec "test -f /etc/os-release"; then
  echo "os-release present"
else
  case $? in
    124) echo "timeout — device unresponsive"; exit 1 ;;
    4)   echo "device disconnected"; exit 1 ;;
    *)   echo "file missing (rc=$?)" ;;
  esac
fi

UPTIME=$(tether -d $D exec "cut -d. -f1 /proc/uptime" --json | jq -r .output)
echo "uptime: ${UPTIME}s"
```

**What's happening:** `exec` runs `<cmd>` on a device already sitting at a
POSIX-ish shell (busybox/dash/bash/hush) and exits with **the device
command's own status**, exactly like `ssh` — so `if tether exec "..."` reads
naturally. `--json` gives you `{output, exit_code, duration_ms}`; pipe to
`jq -r .output` for scripting, never scrape the human-readable form.
`exec`'s default timeout is 5000ms (`--timeout-ms` to change it); other exit
codes to expect from tether itself (not the device command) are `124`
(timeout), `4` (device disconnected), and `8` (device ran the command but
its shell couldn't report a numeric `$?` — see `docs/EXEC_NONPOSIX_SHELLS.md`).
For megabyte-scale output, filter on the device side (`| tail`, `| grep`)
rather than relying on `--max-output-bytes` (default 65536 for `exec`) to
save you.

### 4. Change serial settings live

**Goal:** bump the baud rate mid-session, or set per-device defaults so you
never have to pass `-b` again.

Live, against a running daemon:

```sh
tether -d a35 config                  # show current baud/framing/flow
tether -d a35 config --baud 921600    # apply and print the new state
```

Per-device, at daemon startup (`-D` spec, comma-separated `key=value`):

```sh
tetherd -D 'a35=/dev/ttyUSB0,baud=921600,parity=odd,data-bits=8,stop-bits=1,flow=none'
```

**What's happening:** `config` with no flags reads live state via `status`;
with `--baud`/`--data-bits`/`--parity`/`--stop-bits`/`--flow` it applies a
partial update (`set_device` RPC) to the *already-open* handle — no
reconnect, no dropped bytes in flight. The `-D` inline keys are the startup
equivalent: anything you don't set falls through to the daemon's global
`--baud`/`--parity`/etc. flags. Changing baud on a device backed by a PTY (see
recipes 11–15) is a no-op — a PTY has no real UART; change the daemon's
config instead.

---

## Boards & consoles

### 5. U-Boot: register the console, then just use `exec`/`run`

**Goal:** stop hand-tuning `--newline`/`-u` on every single command against a
U-Boot prompt.

```sh
tetherd -D 'a35=/dev/ttyUSB0,shell=uboot,prompt==> '
tether -d a35 exec "mdio list"          # CR-only framing, forced
tether -d a35 run "printenv"            # -u defaults to the registered prompt
```

Set-and-verify, without `shell=` registered (fall back to `run` with explicit
`-u`/`--newline`):

```sh
PROMPT=$(tether -d a35 sync --idle-ms 500 --timeout-ms 3000)
tether -d a35 --json run "setenv bootdelay 5" -u "$PROMPT" --literal --newline cr | jq -r .output
tether -d a35 --json run "saveenv"           -u "$PROMPT" --literal --newline cr | jq -r .output
tether -d a35 --json run "printenv bootdelay" -u "$PROMPT" --literal --newline cr | jq -r .output
```

**What's happening (verified against real U-Boot hardware — see
`docs/EXEC_NONPOSIX_SHELLS.md`):** `shell=uboot` forces CR-only line framing
for `exec`/`run` on that device and defaults `newline=cr`. **Never send
`crlf` to U-Boot** — its CLI executes the command on the CR and then repeats
the *previous* command when the trailing LF arrives as an empty line
(double execution). `exec` works against hush-enabled U-Boot (the mainline
default for years) because the wrapper is a single line with no temp
variable assignment — U-Boot has no bare `x=0` syntax, which is exactly what
breaks a naïve wrapper. A U-Boot built *without* hush can't run `;`-joined
commands at all; fall back to `run`/`send`/`expect` there. `prompt=` becomes
the default `-u` for both `run` and `sync`, so you stop re-deriving the
regex by trial and error every session.

### 6. Raw / non-shell consoles: `shell=none`, discover the prompt, `run`

**Goal:** drive a bootloader mid-boot, a login prompt, or a vendor MCU
monitor — anything with no shell to run `echo`/`$?`.

```sh
tetherd -D 'a35=/dev/ttyUSB0,shell=none'

# exec refuses immediately instead of timing out:
tether -d a35 exec "version"
# → error: device registered shell=none — use run/send/expect

# Find the prompt once, then use it:
PROMPT=$(tether -d a35 sync --idle-ms 500 --timeout-ms 3000)
tether -d a35 --json run "version" --newline cr -u "$PROMPT" --literal --timeout-ms 3000
```

**What's happening:** `sync` sends a `\r`, waits for the device to go idle,
and returns the last non-empty line as a prompt candidate — the standard way
to discover an unfamiliar console without hard-coding a regex. `run` is one
daemon-side transaction: it holds the writer lock, sends `<data>` plus your
chosen `--newline`, and waits for `-u <pattern>` with a hard `--timeout-ms`.
Add `--literal` since prompts routinely contain regex metacharacters (`.`,
`$`, `>`). `shell=none` exists so `exec` fails fast with the right recipe
instead of burning a 5-second timeout on a console it can never parse `$?`
from.

### 7. Several boards on one host

**Goal:** manage two (or more) UARTs from the same machine without them
interfering.

**One daemon, many devices** (single endpoint, single auth token — pick this
by default):

```sh
tetherd \
  -D 'a35=/dev/ttyUSB0' \
  -D 'esp=/dev/ttyUSB1,baud=921600'

tether list-devices                     # ids, paths, connected state
tether -d a35 status
tether -d esp exec "uname -a"
```

Omitting `-d` against a multi-device daemon returns `-32015 ambiguous_device`
— on an interactive terminal you instead get a numbered picker (Enter selects
the first); scripts and pipes never see the picker, they get the error
directly (or set `--no-interactive`/`TETHER_NONINTERACTIVE=1` to guarantee
that even under a PTY).

**One daemon per board** (process-level isolation — simplest mental model,
more processes):

```sh
tetherd -D /dev/ttyUSB0 --name a35 &
tetherd -D /dev/ttyUSB1 --name esp &

tether --name a35 status
tether --name esp exec "uname -a"
```

**What's happening:** `--name X` is shorthand for `-s /tmp/tetherd-X.sock` on
both sides — it selects which *daemon*, distinct from `-d`/`--device-id`
which selects which *device inside* a daemon. Per-device `-D` keys accepted:
`baud`, `data-bits`, `parity`, `stop-bits`, `flow`, plus the console
personality keys from recipe 5 and the `pty`/`pty=` key from recipe 11.

---

## Remote & automation

### 8. Drive a board attached to a remote host over TCP

**Goal:** an agent, CI runner, or your own laptop needs to reach a board
physically plugged into a different machine.

```sh
# On the machine with the USB serial adapter:
tetherd -D /dev/tty.usbserial-XXXX -b 115200 --tcp --auth-token MYSECRET
# bare --tcp (no token) binds 0.0.0.0:5557 and auto-generates + prints a token

# Anywhere else:
export TETHER_AUTH_TOKEN=MYSECRET
tether -s tcp://daemon-host:5557 status
tether -s tcp://daemon-host:5557 -d a35 exec "uname -a"
```

**What's happening:** UDS connections are authenticated by the OS (file
permissions on `/tmp/tetherd.sock`); TCP connections always require a token,
checked on the first `hello`. A daemon can listen on both transports at
once (`-s /tmp/tetherd.sock` *and* `--tcp`). Security note: bare `--tcp`
binds `0.0.0.0` — reachable from anywhere that can route to the host. Use
`--tcp 127.0.0.1:5557` for loopback-only (e.g. you're tunneling in over SSH
yourself), and treat the token like a password — it's the only thing
standing between the network and your serial console. There's no TLS yet
(see `docs/PROTOCOL.md` §12); for untrusted networks, tunnel through
SSH/WireGuard instead of exposing `--tcp` directly.

### 9. Set up an AI agent on a board

**Goal:** hand a coding agent (Claude Code, Codex, Cursor, a CI bot) the
ability to drive the console without it ever blocking on an interactive
prompt.

```sh
export TETHER_NONINTERACTIVE=1     # or pass --no-interactive per-command
tether agents                      # prints the full cookbook, no daemon needed
```

The essentials the agent needs to internalize: always `-d <id>` (never rely
on a single-device fallback once a second board might show up), always
`--json` for anything it parses, `exec` for shells / `run` for raw consoles,
and an explicit `--timeout-ms` on every call. Full rules, exit codes, and
worked examples live in [`AGENT_USAGE.md`](AGENT_USAGE.md) — that's the
canonical reference an agent (or you, setting one up) should read next; this
recipe is just the two-line bootstrap.

### 10. CI: get a serial port programmatically

**Goal:** a test runner needs a port it can hand to `pyserial` (or any other
tool), with a readiness signal it can trust — no polling, no races.

```sh
tether -d a35 --json pty --link /tmp/ci-a35.pty
# stdout: one NDJSON line once the port is actually usable, flushed:
# {"device":"a35","event":"ready","path":"/tmp/ci-a35.pty","session_id":"..."}
```

Python driving it with `Popen` + a readline-until-ready loop:

```python
import json, subprocess

proc = subprocess.Popen(
    ["tether", "-d", "a35", "--json", "pty", "--link", "/tmp/ci-a35.pty"],
    stdout=subprocess.PIPE, text=True,
)
ready = json.loads(proc.stdout.readline())
port = ready["path"]                  # now safe to open with pyserial

# ... run the test suite against `port` ...

proc.terminate()
proc.wait()
```

Or let `tether` spawn the test script directly, with the path substituted in:

```sh
tether -d a35 pty -- python3 test_script.py {}
# {} → the link path; also exported as $TETHER_PTY inside the child.
# tether pty exits with test_script.py's own exit code — perfect for CI gating.
```

**What's happening:** startup order is a contract specifically so CI doesn't
need a fixed sleep: pty created → link published (atomic `symlink(2)`,
stale-link detection via a `.pid` sidecar) → session attached → path printed
and flushed → bridge starts. Reading that one line (or waiting for the `{}`
form to hand off) is the only synchronization you need. `--json` turns the
bare path into a structured event so you don't have to string-match stdout.

---

## Sharing the port with other tools (virtual ports)

A `tetherd`-owned port can't be opened a second time by another program —
the OS only lets one process hold `/dev/ttyUSB0`. `tether pty` solves this by
creating a **client-side virtual serial port**: a real PTY, bridged to the
device through the normal protocol, that any serial tool can open like a
device node.

### 11. minicom / picocom / screen on a tether-managed board

**Goal:** point an existing terminal tool at the board without taking the
port away from `tetherd`.

```sh
tether -d a35 pty -- minicom -D {}     # {} → substituted with the port path
tether -d a35 pty -- picocom {}
tether -d a35 pty -- screen {} 115200
```

**What's happening:** the virtual port lives exactly as long as the child
process; `tether pty` exits with the child's own exit code when it quits.
**Do not** point two tools at the same virtual port — the kernel splits a
single PTY's bytes between simultaneous readers (measured: a 200-byte write
arrived as 128 bytes on one reader and 72 on the other). If you want a second
tool watching concurrently, run a second `tether pty` (its own `--link`) —
each gets a full, independent copy of the stream from the daemon.

### 12. pyserial against a virtual port

**Goal:** drive the board from a Python test harness that expects a
`pyserial`-compatible device node.

```sh
tether -d a35 pty --link /tmp/a35.pty &
sleep 1   # or use the --json readiness line from recipe 10 instead of sleeping
```

```python
import serial, time

p = serial.Serial("/tmp/a35.pty", 115200, timeout=2)
p.write(b"echo hello\n")
time.sleep(0.5)
print(p.read(4096))
```

**What's happening (verified working end-to-end against a live bridge):**
`pyserial` has no idea it isn't talking to a real termios device — writes go
through the PTY master, across the wire to the daemon, out the real port,
and the reply comes back the same way. The baud rate `pyserial` sets on the
virtual port is a no-op (see recipe 4 — the real rate is the daemon's
config).

### 13. Observation-only port for a colleague

**Goal:** let someone watch the board's output through their own tool
without any risk of them accidentally typing into your session.

```sh
tether -d a35 pty --link /tmp/a35-readonly.pty --read-only
```

**What's happening (verified):** bytes typed into a `--read-only` virtual
port never reach the device — they're counted and dropped, with a
rate-limited stderr warning (`read-only: dropped N bytes typed into <path>`).
Everything the device *emits* still flows through normally. Handy
for a demo, a training session, or handing a log-only view to a teammate
without a second daemon or a lock.

### 14. A remote board as a local port

**Goal:** the board is physically on a lab host; you want it to behave like
a port on your own laptop, for a tool that only knows how to open device
files.

```sh
tether -s tcp://lab-host:5557 -d a35 pty -- minicom -D {}
```

**What's happening:** the PTY is created client-side, so this works
identically over TCP — the daemon relays bytes across the network, and
`minicom` (or any other tool) never knows the difference. This is arguably
the single most useful trick in this document: any serial tool, anywhere,
against any board `tetherd` can reach. Expect ordinary network latency added
to every round-trip — fine for interactive use and most automation, less
fine for tight timing-sensitive protocols.

---

## Flashing (advanced)

### 15. Flash through a shared port

**Goal:** run a UART flasher (esptool-style tool) against a board whose port
is normally shared, without another session's bytes corrupting the flash
stream, and with the board actually entering download mode.

```sh
# 1. Reset the board into download mode — on the REAL port, not the PTY:
tether -d a35 reset --esp32
# or, for other wirings, an explicit sequence:
tether -d a35 reset --seq "dtr=0 rts=1 wait=100 dtr=1 rts=0 wait=50 dtr=0"

# 2. Flash against the virtual port, exclusive-locked, tool's auto-reset OFF:
tether -d a35 pty --lock -- esptool --port {} --before no_reset write_flash 0x0 firmware.bin
```

**Why the two-step dance:** a PTY carries data only — **no modem control
lines and no real baud rate** (measured: macOS PTYs return `ENOTTY` for every
modem-control ioctl on both ends of the pair; this is an OS limitation, not a
tether bug). A flasher's usual DTR/RTS auto-reset therefore can't reach the
board through `tether pty`. `tether reset` sidesteps this by driving DTR/RTS
directly on the daemon's real serial handle — independent of whatever PTY
bridge is active — then you point the flasher at the virtual port with its
own auto-reset disabled (`esptool --before no_reset`, or the equivalent flag
for your tool) so it doesn't try (and fail) to reset the board itself.

**Lock semantics (verified: exit 6 while held, exit 0 once released):**
`--lock` takes the device's exclusive writer lock for the life of the `pty`
session. While held, `send` from any *other* session — a human's stray
keystroke, a second script — fails immediately with `-32004 lock_contention`
(process exit **6**) instead of interleaving into the flash stream; the
daemon-side bridge also drops (not queues) bytes from anything else trying
to write. The lock releases automatically on `unlock`, the session
detaching, or the connection closing — so a crashed flashing client can't
strand the device locked forever.

**What does *not* work through a PTY, today:** a flasher's own auto-reset
(use `tether reset` instead, as above); a flasher that switches baud
mid-stream expecting the port to actually retime (change baud with `tether
config --baud` on the daemon side if the tool supports a fixed external
rate, or flash directly against the real port if it doesn't support that).

### 16. Kick a wedged port

**Goal:** `status` says `connected: true` but nothing responds — a USB
driver hiccup, a half-finished reset, or a daemon that's lost its socket.

```sh
tether -d a35 status                    # connected:true but hung?
tether -d a35 reconnect                 # close + reopen the real port
tether -d a35 reconnect --nowait        # don't block waiting for it to come back

# Handing the port to another tool temporarily (flashrom, a vendor tool):
tether -d a35 disconnect                # closes the port, pauses auto-reconnect
# ... run the other tool directly against /dev/ttyUSB0 ...
tether -d a35 connect                   # daemon reopens and resumes

# Daemon seems dead but the socket file is still there:
ls -la /tmp/tetherd.sock
tether status                           # exit 3 (connection error) confirms it's stale
rm -f /tmp/tetherd.sock                 # then start a fresh tetherd
```

**What's happening:** `reconnect` drops and reopens the device handle inside
the running daemon — no client needs to restart. `disconnect`/`connect` is
the operator-driven version: `disconnect` parks the port and marks it
`explicitly_disconnected` so auto-reconnect doesn't fight you while another
program borrows `/dev/ttyUSB0`; `connect` clears that flag and forces an
immediate reopen (waits up to 2s, reflects the final state in its result).
`lines` (`CTS`/`DSR`/`RI`/`DCD`) can help diagnose whether the cable itself
is the problem — but only against a real serial backend; a PTY-backed device
returns `-32007 unsupported_serial_op` for `break`/`dtr`/`rts`/`lines`
(verified), same as it would for a flasher expecting real modem lines
through a virtual port (recipe 15).

---

Recipe didn't work as written? Check
[`TROUBLESHOOTING.md`](TROUBLESHOOTING.md) before assuming it's wrong —
most surprises trace back to a wrong `--newline`, a missing `-d`, or a stale
socket file.
