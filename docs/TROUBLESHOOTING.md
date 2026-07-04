# Troubleshooting

Symptom-first. Find what you're seeing, do the fix. For flag-by-flag details
see [docs/CLI_REFERENCE.md](CLI_REFERENCE.md); for task walkthroughs see
[docs/COOKBOOK.md](COOKBOOK.md); for a first-time setup see
[docs/GETTING_STARTED.md](GETTING_STARTED.md) or the top-level
[README.md](../README.md).

## Connecting

### "cannot connect to /tmp/tetherd.sock — socket file does not exist (daemon not running?)"

**Cause:** No `tetherd` has ever bound that path. Nobody started a daemon (or
you're pointing at the wrong `-s`/`--name`).

**Fix:** Either skip the daemon entirely for one-off use, or start one:

```sh
tether /dev/tty.usbserial-XXXX                        # one-off, no daemon needed
tetherd -D /dev/tty.usbserial-XXXX -b 115200           # long-lived, multi-client
tetherd -D /dev/tty.usbserial-XXXX -b 115200 --tcp     # + remote agents
```

`tether` prints exactly this (both example commands included) whenever it
hits this error — it's not abbreviated in the real output.

### "cannot connect to /tmp/tetherd.sock — connection refused (stale socket from a crashed daemon?)"

**Cause:** The socket *file* still exists but nothing is listening on it — the
daemon that created it died (crash, `kill -9`, OOM) without cleaning up.

**Fix:** Same commands as above — starting a new `tetherd` on that path
recreates the socket. If you want to confirm first: `ls -la
/tmp/tetherd.sock` and `lsof /tmp/tetherd.sock` (empty output = confirmed
stale); `rm /tmp/tetherd.sock` is safe once you've confirmed nothing holds it.

### Wrong `-s`/`--name`

**Cause:** `--name X` on the client only reaches a daemon started with the
*same* `--name X` (it's shorthand for `-s /tmp/tetherd-X.sock`) — a mismatch
looks like any other connection failure, not a distinct error.

**Fix:** Client and daemon must agree on the exact `--name` (or both omit
it). When unsure what a running daemon is called: `ls /tmp/tetherd*.sock`
and match the suffix.

### TCP: "tether: unauthorized: auth token missing or incorrect" (exit 7)

**Cause:** The daemon has `--tcp` on and requires a token; you connected
without `--auth-token`/`TETHER_AUTH_TOKEN`, or with the wrong one.

**Fix:**

```sh
export TETHER_AUTH_TOKEN=<token-the-daemon-printed>
tether -s tcp://<host>:5557 status
```

The token is whatever you passed the daemon with `--auth-token`, or (if you
didn't) the random one it printed to stderr on startup — look in the
daemon's log/terminal, or the "auth token: ..." line of its startup banner.
Every TCP connection needs one; UDS connections never do (they're
authenticated by OS file permissions instead).

### TCP unreachable — "cannot connect to tcp://host:port — Connection refused (os error 61)" / timeout

**Cause:** Either nothing is listening on that host:port (`tetherd` isn't
running, isn't using `--tcp`, or bound a different port/interface — e.g.
`--tcp 127.0.0.1:5557` only accepts loopback, not remote connections), or a
firewall/security group is dropping the connection.

**Fix:** On the daemon host, confirm it's actually listening: the startup
banner lists every reachable IP under "Listening: tcp ...". If you used a
bare `--tcp` it binds `0.0.0.0:5557` (all interfaces) — check the *port*
first, then the firewall. `tether` prints this same remediation whenever the
TCP dial itself fails (as opposed to a token being rejected, which connects
fine and fails at `hello`).

## Devices

### `-32015` / "daemon serves N devices; pass device_id to select one" (exit 2)

**Cause:** The daemon manages more than one device (multiple `-D` flags) and
you didn't pass `-d`/`--device-id`. This is the exact message the daemon
sends — not the generic "ambiguous device" text you might see quoted
elsewhere.

**Fix:**

```sh
tether --json list-devices     # see every id
tether -d board0 exec "..."    # target one explicitly
```

In an interactive terminal (not `TETHER_NONINTERACTIVE=1`, not a script), a
few commands (not `status`/`list-devices`/`ports`/`agents`) show an
interactive picker instead of erroring — see "Agent hangs" below for why
that's dangerous for automation.

### Agent/script hangs instead of erroring on ambiguous device

**Cause:** `tether` detected an interactive terminal and is showing a device
picker menu, waiting for a keypress that will never come under a harness-
allocated PTY.

**Fix:** Always set `export TETHER_NONINTERACTIVE=1` (or pass
`--no-interactive`) for any agent/script/CI context. With it set, the same
situation fails fast with the `-32015` error above instead of prompting.
Plain pipes (no PTY at all) are already treated as non-interactive, but a
PTY-allocated harness is not — set the variable explicitly, don't rely on
detection.

### `status` shows the default device's info instead of the one I want

**Cause:** You omitted `-d`. Without it, `status`'s top-level
`device`/`buffer` fields describe the daemon's *default* device (the first
`-D` given at daemon startup). With `-d <id>`, they describe that device —
and an unknown id fails with `device "<id>" not found (daemon has: …)`.

**Fix:** Pass `-d`, or read the full `devices` array:

```sh
tether -d board0 --json status | jq .device
tether --json list-devices | jq '.devices[] | select(.id == "board0")'
```

### Device disconnected (exit 4)

**Cause:** Either the USB device was physically unplugged / the kernel node
disappeared, or someone ran `tether disconnect` and nobody has run `tether
connect` since.

**Fix:** Check which case you're in:

```sh
tether --json list-devices | jq '.devices[] | {id, connected, explicitly_disconnected}'
```

- `explicitly_disconnected: true` → `tether -d <id> connect` reopens it.
- `explicitly_disconnected: false`, `connected: false` → the daemon's
  auto-reconnect loop is retrying on its own; re-plug the device and it
  should recover. `tether -d <id> reconnect` forces an immediate retry
  instead of waiting for the daemon's own backoff.
- For a script that should ride out a transient disconnect instead of
  failing, add `--auto-reconnect` to the command — it retries the RPC once
  after a successful reconnect.

### "No such file or directory" for the device path

**Cause:** The port isn't plugged in, was unplugged after the daemon
started, or (macOS) its device node **changed name** between plugs. Cheap
USB-serial chips without a unique per-device serial number (many CP2102/
CH340 boards) get an auto-incremented suffix (`usbserial-0001`, ...) that can
land on a different actual board depending on plug order. A chip with a real
per-unit serial number (most FTDI adapters) gets a name that stays stable
across replugs.

**Fix:** Don't hardcode the path in long-lived scripts — resolve it at the
start of each session with `tether ports --json` (vid/pid/serial included).
If identical boards keep swapping suffixes, disambiguate on `serial_number`
(present on FTDI, usually absent on CP2102/CH340 clones) or track by which
physical USB hub port a board is in instead of by device name.

### `cu.*` vs `tty.*` on macOS

**Cause:** macOS exposes every serial device twice: `/dev/cu.*` (call-out —
the node to open for outgoing connections) and `/dev/tty.*` (dial-in,
blocks open waiting for carrier-detect — essentially never what you want for
a USB-serial adapter). They refer to the same underlying hardware.

**Fix:** Always use `/dev/cu.*`. `tether`'s own interactive port picker
already filters out the `tty.*` duplicates for you (`discover_usb_ports`
prefers `cu.*` and drops the matching `tty.*` entry), but if you're
hardcoding a path yourself, use `cu.`.

### Two daemons fighting over the same port

**Cause:** You ran `tether -D /dev/ttyUSB0 ...` (or the bare-path shorthand)
against a device a daemon is *already* managing. Two processes opening the
same serial node at once corrupts both sessions.

**Fix:** Nothing to do — this is handled for you. `tether -D`/bare-path
first probes `/tmp/tetherd*.sock` for a daemon that already owns that exact
path; if it finds one, it prints:

```
tether: device /dev/ttyUSB0 is already managed by daemon at /tmp/tetherd.sock (id: ttyUSB0)
tether: attaching as a client — no new daemon spawned, the existing session keeps running.
```

and attaches as a plain client instead of spawning a second daemon. You
don't need a `pgrep tetherd` check before scripting this.

### Garbled / random bytes

**Cause:** Wrong baud rate (most common), or mismatched parity/data/stop
bits between `tetherd`'s config and the device.

**Fix:**

```sh
tether -d <id> status          # shows the configured baud/framing
tether -d <id> config --baud 115200   # change it live, no daemon restart
```

## Commands

### `exec` times out

**Cause, in order of likelihood:**
1. The device isn't at a shell prompt at all (still booting, stuck at a
   bootloader menu, or a raw/vendor monitor).
2. It's U-Boot **without** the hush parser — such a build can't run `;`,
   quotes, or `$?` expansion, so `exec`'s wrapper never completes.
3. The device was registered `shell=none`.

**What you'll actually see** — `tether` inspects the buffered pre-timeout
output and picks the most specific message it can:

- Buffer contains `Unknown command`: `tether: exec timed out after <N>ms —
  the device looks like a U-Boot / raw console (saw "Unknown command"). Try:
  tether -d <id> run "<cmd>" -u "=> " --newline cr or set shell=uboot on the
  device`.
- Device registered `shell=uboot` but no end-marker ever appeared: `tether:
  exec timed out after <N>ms on a shell=uboot device — no end marker seen. A
  U-Boot built without the hush parser can't run exec; fall back to ... run
  ...`.
- Otherwise: generic `tether: exec timed out after <N>ms — no end-marker
  seen. Is the device at a shell prompt? ...`.
- Device registered `shell=none`: `exec` refuses **immediately** (no
  timeout wait) with the `run`/`send`+`expect` recipe.

**Fix:** For a raw console, use `run` directly instead of `exec`:

```sh
tether -d <id> run "<cmd>" -u "<prompt-regex>" --newline cr --json
```

Or register the device's console personality once so both `exec` and `run`
"just work" from then on: `tetherd -D board=/dev/ttyUSB0,shell=uboot,prompt='=> '`.

### `exec` exit 8 / `exit_code: null`

**Cause:** The command ran and its output was captured correctly, but the
device shell never substituted a numeric `$?` into the end marker (a
non-POSIX console — U-Boot without hush is the classic case; the literal
token `$__trc` or similar shows up in the raw buffer instead of a number).
`tether` never fabricates a `0` here — it's a distinct signal from success.

**Fix:** See [docs/EXEC_NONPOSIX_SHELLS.md](EXEC_NONPOSIX_SHELLS.md) for the
full failure analysis. Practically: register `shell=uboot` (or `shell=none`
for a truly raw console) so `exec` either succeeds cleanly or refuses
up-front instead of guessing.

### `run` times out with no match

**Cause, in order of likelihood:**
1. Forgot `--newline` — the command was sent with no line terminator, so
   the device never saw a complete line.
2. Wrong `-u`/`--until` pattern (prompt regex doesn't match reality, or
   needs `--literal` because it contains regex metacharacters like `.`/`$`).
3. `-u` was omitted and the device has no `prompt=` configured — this one
   actually fails fast with an explicit error, not a timeout:
   `tether: run needs a --until/-u pattern (none given and the device has no
   prompt= configured). e.g. tether ... run "<data>" -u "<prompt-regex>" or
   set prompt= on the device`.

**Fix:**

```sh
tether -d <id> sync --idle-ms 500 --timeout-ms 3000   # discover the real prompt
tether -d <id> run "<cmd>" -u "<prompt>" --literal --newline cr
```

`sync` is cheap — use it instead of guessing.

### U-Boot runs every command twice

**Cause:** `--newline crlf` toward U-Boot. Its CLI executes the line on CR,
then treats the trailing LF as a second, empty command line — which repeats
the *previous* command.

**Fix:** Use `--newline cr` for anything U-Boot. Or register the device once
with `shell=uboot`, which forces CR-only framing for `exec`/`run`
automatically (and warns on stderr if you explicitly pass a conflicting
`--newline` to `exec`).

### `expect` matches stale output from a previous command

**Cause:** `expect` (and hand-composed `send` + `expect`) has no protection
against multiple clients interleaving writes, or against matching leftover
output from before your `send`. It's not a single atomic transaction.

**Fix:** Use `run` (or `exec` for a shell) instead — both are a single
server-side transaction that captures the buffer position *before* writing,
so the match window can't include stale pre-existing output or another
client's interleaved bytes.

## Locking & sharing

### `send` fails with exit 6: "device is locked by another session"

**Cause:** Another session holds the device's **exclusive** writer lock
(via `tether pty --lock` or the raw `lock` RPC) — typically someone
mid-flash. `tether` exits 6 (`lock_contention`) and prints one stderr line:
`tether: device is locked by another session (flashing?) — try again after
it unlocks`.

**Fix:** Check who holds it, then either wait or preempt:

```sh
tether -d <id> status --json | jq .lock          # {"holder_session_id": "..."}
```

The lock releases automatically when the holding session detaches or its
connection closes (a crashed flashing client can't strand the device
locked), so waiting is often enough. To not wait, a `run` (not `send`) can
pass `--preempt force` to seize the writer lock outright — use this
deliberately, it aborts whatever the current holder was doing.

`run --preempt fail` returns the same exit 6 for the same reason
(`--preempt fail` is what makes contention return immediately instead of
queuing).

### Interleaved/garbled output when two people type at once

**Cause:** Expected, not a bug. Plain `send`/interactive `shell` typing does
not take the writer lock — every attached session's bytes reach the device
in the order they were sent, but two humans typing at the same time will
naturally produce interleaved characters on the wire, same as it would with
two people literally sharing one keyboard.

**Fix:** Coordinate out of band (who's driving right now), or have the
non-driving session attach read-only. `tether tail` is always read-only and
never contends with anyone. For a virtual port, `tether pty --read-only`
gives a tool an observe-only view.

## Virtual ports (`pty`)

### Two tools on one virtual port split the byte stream

**Cause:** A single virtual port has one pty slave; if two processes open
and read it simultaneously, the kernel divides incoming bytes between them
(measured roughly 128/72 of 200 bytes in one test) — neither tool sees the
complete stream.

**Fix:** One `tether pty` (with its own `--link`) per concurrent tool. Never
point two tools at the same link path at the same time.

### Baud rate set on a virtual port has no effect

**Cause:** A pty has no UART — there's no real "baud" to set. Whatever a
tool configures on the virtual port (e.g. pyserial's `baudrate=`) is silently
a no-op.

**Fix:** The real transfer rate is whatever the daemon's device config says.
Change it with `tether -d <id> config --baud <N>` (or the `-D ...,baud=`
inline setting at daemon startup) if the tool actually needs a different
rate.

### DTR/RTS toggles from a flashing tool don't reset the board

**Cause:** A pty carries no modem control lines — a flasher's
auto-reset-via-DTR/RTS sequence has nothing to act on when it's pointed at
`/tmp/tether-<id>.pty`.

**Fix:** Drive the reset on the **real** port with `tether reset` first (or
in place of the tool's own auto-reset), then flash with the tool's
no-auto-reset option pointed at the virtual port:

```sh
tether -d <id> reset --esp32                                   # or a custom --seq
tether -d <id> pty --lock -- esptool --port {} --before no_reset write_flash ...
```

### Stale `/tmp/tether-*.pty` link (and `.pty.pid`) after `kill -9`

**Cause:** A `tether pty` process that's killed hard (SIGKILL, OOM, crash)
can't run its normal cleanup, so the symlink and its `<link>.pid` sidecar
are left behind.

**Fix:** Usually nothing — the *next* `tether pty --link <same path>`
invocation checks the sidecar's pid, confirms the old process is gone
(`kill(pid, 0)` liveness probe), and reclaims + retries automatically. If
you want to clean up by hand instead, both files are safe to remove once
you've confirmed the pid in `<link>.pid` isn't running:

```sh
cat /tmp/tether-<id>-<n>.pty.pid    # the pid it thinks owns the link
ps -p $(cat /tmp/tether-<id>-<n>.pty.pid)   # confirm it's actually gone
rm /tmp/tether-<id>-<n>.pty /tmp/tether-<id>-<n>.pty.pid
```

### "tool not reading `<link>`; dropped N bytes"

**Cause:** Bytes came from the device toward the virtual port faster than
the attached tool was reading them, and the bridge's write to the pty
master is timeout-bounded specifically so a stopped/stuck tool can't wedge
the whole bridge. Past that timeout, the bytes are dropped and counted
instead of blocking forever.

**Fix:** This is a warning, not a hard failure — but it means the tool
attached to the virtual port missed data. Check whether the tool is still
alive and actually reading (not paused at its own internal breakpoint,
blocked on its own I/O, etc.). A read-only observer (`tether pty
--read-only`) reports the mirror-image counter — `"read-only: dropped N
bytes typed into <link>"` — for bytes typed into the port that never reached
the device.

### esptool auto-reset doesn't work through a virtual port

**Cause:** Same root cause as the DTR/RTS section above — esptool's default
`--before default_reset` (or `hard_reset`) drives DTR/RTS, which a pty can't
carry.

**Fix:** `tether reset --esp32` on the real port, then esptool with
`--before no_reset` against the virtual port. See
[docs/COOKBOOK.md](COOKBOOK.md) for the full recipe.

## Daemon lifecycle

### The daemon keeps running after I close my terminal

**By design.** A `tetherd` you start explicitly (not the embedded one from
`tether -D`/bare-path standalone mode) is a long-lived background process —
closing the terminal it was launched from doesn't stop it (especially if you
backgrounded it with `&` or `nohup`).

**Fix:** `pkill -f tetherd` stops every daemon on the box; to stop just one,
find its pid (`ps aux | grep tetherd`, or the pid it logs at startup) and
`kill` it. The embedded daemon from `tether -D`/bare-path *does* die with its
client automatically — that's the one difference between the two modes.

### Leftover sockets/links after a crash

**Cleanup one-liner** for standalone-mode leftovers (`tether -D`'s embedded
daemon names its socket/log `/tmp/tether-<pid>-<nonce>.{sock,log}`, and
`tether pty` names its link `/tmp/tether-<id>-<n>.pty` + `.pid`):

```sh
rm -f /tmp/tether-*.sock /tmp/tether-*.log /tmp/tether-*.pty /tmp/tether-*.pty.pid
```

Confirm nothing is actually using them first (`lsof <path>` on a `.sock`, or
check `.pid` sidecars per the pty section above) if any daemon might still
be alive — this removes files, it doesn't check liveness for you.

### Where do logs go?

- The human-readable **startup banner** (device summary, listening
  addresses, auth token) always goes to **stderr**.
- Structured **tracing logs** (`INFO`/`WARN`/…) go to **stdout** by default,
  at `info` level. Control the level with the standard `RUST_LOG` env var
  (e.g. `RUST_LOG=debug tetherd -D ...`) — there's no `tetherd`-specific log
  env var.
- If you redirected both to the same file (`tetherd ... > tetherd.log
  2>&1`), banner and tracing lines interleave in that one file.

## Exit codes (quick reference)

| Code | Meaning |
|---|---|
| `0` | ok / matched |
| `1` | usage error (bad flags/args) |
| `2` | protocol/RPC error (e.g. `-32015 ambiguous_device`, `-32009 device_not_found`) |
| `3` | connection error (couldn't dial the daemon) |
| `4` | device disconnected |
| `5` | buffer overflow (no match within the 1 MiB search window; stderr explains) |
| `6` | lock contention — one-line explanation on stderr |
| `7` | unauthorized (TCP auth token missing/wrong) |
| `8` | `exec` ran, device shell reported no numeric status |
| `124` | timeout |

`exec` and `pty -- <EXEC>` are exceptions: they pass through the device
command's / child process's own exit status instead of using this table.
Full flag/default reference: [docs/CLI_REFERENCE.md](CLI_REFERENCE.md).
