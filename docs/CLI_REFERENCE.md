# CLI Reference

Complete flag-by-flag reference for `tether` (client) and `tetherd` (daemon).
Generated against the binaries — every flag, default, and value set below is
what `--help` actually prints, not what an older doc says it prints.

New to the tool? Start with [docs/GETTING_STARTED.md](GETTING_STARTED.md) or
the top-level [README.md](../README.md). Task-oriented walkthroughs (flashing
through a shared port, CI with a virtual serial port, multi-board setups) live
in [docs/COOKBOOK.md](COOKBOOK.md). This page is the reference, not the tour.

## Synopsis

```
tether [OPTIONS] [COMMAND]
tether <PATH> [COMMAND]            # bare-path shorthand — see "Standalone mode" below
tetherd --device <[ID=]PATH[,KEY=VAL...]> [OPTIONS]
```

`tether` with no command drops into the interactive `shell`. Every subcommand
accepts the same global flags (endpoint, device target, output, TCP auth,
standalone mode) in addition to its own; they're documented once here instead
of repeated per command.

### Endpoint resolution order

1. `-s, --socket <SOCKET>` if given — a UDS path (`/tmp/tetherd.sock`) or
   `tcp://host:port` / `tcp:host:port` for a remote daemon.
2. `--name <NAME>` if given — shorthand for `-s /tmp/tetherd-<NAME>.sock`.
   Mutually exclusive with `--socket` (for TCP, pass `-s tcp://...` directly).
3. Default: `/tmp/tetherd.sock`.

### Bare-path shorthand

```sh
tether /dev/ttyUSB0 [COMMAND...]
```

is rewritten to `tether -D /dev/ttyUSB0 [COMMAND...]` before parsing (the
client detects a first argument that isn't a known subcommand and looks like a
device path). It brings up a private `tetherd`, runs the command (or drops
into `shell` if none was given), and tears the daemon down on exit — the same
UX as `tio /dev/ttyUSB0`. If a daemon is already managing that exact path, it
attaches to it as a client instead of spawning a second one (a notice prints
to stderr) — safe to script without checking `pgrep tetherd` first.

## Global flags

| Flag | Default | Meaning |
|---|---|---|
| `-s, --socket <SOCKET>` | `/tmp/tetherd.sock` | Daemon endpoint: UDS path or `tcp://host:port`. |
| `--name <NAME>` | — | Connect to `/tmp/tetherd-<NAME>.sock`. Conflicts with `--socket`. |
| `-d, --device-id <ID>` | — | Device to target. Required on multi-device daemons (else `-32015 ambiguous_device`); optional on single-device daemons. |
| `--json` | `false` | Emit raw JSON instead of human-readable text. Always use this for scripts/agents. |
| `--auto-reconnect` | `false` | Auto-reconnect + retry once on a `device_disconnected` reply. |
| `--no-interactive` | `false` | Never show an interactive port/device picker; fail with the normal error instead. Same as `TETHER_NONINTERACTIVE=1`. |
| `--auth-token <TOKEN>` | — | Auth token for TCP transport. Same as `TETHER_AUTH_TOKEN` env var. |
| `-D, --device <DEVICE>` | — | Standalone mode: spawn a private `tetherd` for this device (accepts the same `[ID=]PATH[,KEY=VAL...]` spec as `tetherd -D`). Mutually exclusive with `-s tcp://...` / explicit `-s`. |
| `-b, --baud <BAUD>` | `115200` | Baud for standalone mode (`-D`) only. |
| `--tcp[=<HOST:PORT>]` | off | Standalone mode only: also expose the embedded daemon over TCP. Bare `--tcp` binds `0.0.0.0:5557`; use `--tcp=HOST:PORT` for a custom bind (the `=` is required so a following subcommand name isn't parsed as the value). Auto-generates and prints an auth token if `--auth-token` isn't given. Dies when you quit the standalone session. |

`-h`/`--help` and `-V`/`--version` are also global (clap-provided).

## Commands

### Interactive

#### `shell`

Raw-mode interactive terminal. Forwards stdin to the device; renders device
output on stdout. Default when no subcommand is given.

```
tether shell [OPTIONS]
```

| Flag | Default | Meaning |
|---|---|---|
| `--from <FROM>` | `now` | Replay position on attach: `start` \| `now`. |

Escape prefix is Ctrl-A: `Q` quit, `C` show config, `V` list ports, `B`
BREAK, `D` toggle DTR, `R` toggle RTS, `L` modem status, `?` help, Ctrl-A
again sends a literal Ctrl-A to the device.

```sh
tether shell                    # attach to the default daemon
tether shell --from start       # replay the buffer before following live
```

#### `tail`

Stream live device output to stdout, like `tail -f`. Read-only — doesn't take
the writer lock, so it runs alongside `send`/`exec`/`shell`/other `tail`s.
Device bytes go to stdout; connect/disconnect notices go to stderr.

```
tether tail [OPTIONS]
```

| Flag | Default | Meaning |
|---|---|---|
| `--from <FROM>` | `now` | `now` = only new output; `start` = replay the buffer first, then follow. |

```sh
tether -d board0 tail > console.log &     # background logger
tether -d board0 tail --from start        # see what's already buffered
```

#### `sync`

Send CR and wait until the device goes idle; print the last line as a prompt
candidate. Useful for detecting an unknown prompt before a `run` sequence —
see [docs/COOKBOOK.md](COOKBOOK.md) for the full pattern.

```
tether sync [OPTIONS]
```

| Flag | Default | Meaning |
|---|---|---|
| `--idle-ms <MS>` | `300` | Treat the device as idle once this many ms pass with no new output. |
| `--timeout-ms <MS>` | `2000` | Give up if the device never goes idle. |

```sh
PROMPT=$(tether -d board0 sync --idle-ms 500 --timeout-ms 3000)
```

### Scripted RPCs

#### `send`

Send data to the device. Fire-and-forget — does not wait for a response and
does not take the writer lock (bytes may interleave with other clients'
writes).

```
tether send [OPTIONS] <DATA>
```

| Arg/Flag | Default | Meaning |
|---|---|---|
| `<DATA>` | — | Bytes to send. |
| `--base64` | off | Treat `<DATA>` as base64. |
| `--newline <NL>` | `none` | Append a terminator: `none` \| `lf` \| `cr` \| `crlf`. |

```sh
tether -d board0 send "version" --newline crlf
```

#### `expect`

Wait until a pattern appears in the device output.

```
tether expect [OPTIONS] <PATTERN>
```

| Arg/Flag | Default | Meaning |
|---|---|---|
| `<PATTERN>` | — | Pattern to wait for (regex unless `--literal`). |
| `--timeout-ms <MS>` | `3000` | Give up after this long. |
| `--literal` | off | Match `<PATTERN>` as a fixed string, not a regex. |
| `--strip-ansi <BOOL>` | `true` | Strip ANSI escapes before matching. |
| `--max-output-bytes <N>` | `8192` | Cap captured pre-match output to the trailing N bytes. |

```sh
tether -d board0 expect "# $" --timeout-ms 3000
```

Prefer `run` over hand-composed `send` + `expect` — see
[AGENT_USAGE.md](AGENT_USAGE.md#race-free-patterns) for why (interleaving
between two clients can corrupt matching).

#### `run`

Atomic send + expect, executed on the server (holds the writer lock for the
transaction — race-free even with other clients attached).

```
tether run [OPTIONS] <DATA>
```

| Arg/Flag | Default | Meaning |
|---|---|---|
| `<DATA>` | — | Data/command to send before waiting for `--until`. |
| `-u, --until <PATTERN>` | device `prompt=`, else required | Pattern that ends the wait (regex unless `--literal`). |
| `--timeout-ms <MS>` | `3000` | Give up waiting for `--until`. |
| `--literal` | off | Match `--until` as a literal string. |
| `--strip-ansi <BOOL>` | `true` | Strip ANSI escapes before matching. |
| `--strip-echo <BOOL>` | `true` | Strip the echoed command line from the response. |
| `--max-output-bytes <N>` | `8192` | Cap captured output to the trailing N bytes. |
| `--preempt <MODE>` | `queue` | Writer-lock contention behavior: `queue` \| `fail` \| `force`. |
| `--newline <NL>` | device `newline=`, else `none` | Terminator to append: `none` \| `lf` \| `cr` \| `crlf`. **Never `crlf` toward U-Boot** — double-executes. |

If `-u`/`--until` is omitted and the device has no `prompt=` configured, `run`
errors with the exact `-u` invocation to use — it never guesses.

```sh
tether -d board0 run "version" -u "# " --literal --timeout-ms 3000
tether -d board0 --json run "printenv" -u "=> " --literal --newline cr
```

#### `exec`

Run a shell command on the device and capture just its output. Wraps
`<DATA>` so the device shell brackets the output with unique begin/end
markers and reports the command's exit status in-band, then returns only the
bytes the command produced — echoed command line dropped even if the
terminal wrapped it. Exits with the **device command's own status**, like
`ssh`. Assumes a POSIX-ish shell (busybox/dash/bash/hush U-Boot) at a prompt;
refuses immediately on a device registered `shell=none`.

```
tether exec [OPTIONS] <DATA>
```

| Arg/Flag | Default | Meaning |
|---|---|---|
| `<DATA>` | — | Command line to run on the device shell. |
| `--timeout-ms <MS>` | `5000` | Give up if the end-marker hasn't appeared. |
| `--strip-ansi <BOOL>` | `true` | Strip ANSI escapes from captured output. |
| `--max-output-bytes <N>` | `65536` | Cap captured output to the trailing N bytes. |
| `--preempt <MODE>` | `queue` | Writer-lock contention behavior: `queue` \| `fail` \| `force`. |
| `--newline <NL>` | device `newline=`, else `cr` | Terminator used to submit the command: `lf` \| `cr` \| `crlf` (no `none` — unlike `send`/`run`). A `shell=uboot` device always forces `cr` regardless of this flag. |

```sh
tether -d board0 exec "uname -a"
tether -d board0 exec "cat /proc/uptime" --json     # {output, exit_code, duration_ms}
if tether -d board0 exec "test -f /etc/os-release"; then echo present; fi
```

See [docs/EXEC_NONPOSIX_SHELLS.md](EXEC_NONPOSIX_SHELLS.md) for the
`exit_code: null` / exit-8 case, and [COOKBOOK.md](COOKBOOK.md) for U-Boot
setup via `shell=uboot`.

### Inspection

#### `status`

Show daemon + device status (buffer occupancy, lock holder, attached
sessions).

```
tether status [OPTIONS]
```

No subcommand-specific flags.

```sh
tether status
tether --json status | jq '{path: .device.path, baud: .device.baud}'
```

With `-d`/`--device-id`, the top-level `device`/`buffer`/`lock`/`sessions`
fields describe **that** device (an unknown id errors, listing the ids the
daemon does have). Without `-d`, they describe the daemon's default device
(the first `-D` given, or the only device). The `devices` array always
carries every device either way:

```sh
tether -d board1 --json status | jq '{path: .device.path, baud: .device.baud}'
tether --json status | jq '.devices[] | select(.id == "board0")'
```

#### `list-devices`

List every device the daemon manages: id, path, serial settings, connected
state, console personality (`shell`/`prompt`/`newline`), and which id is the
default (used when `-d` is omitted on a single-device daemon).

```
tether list-devices [OPTIONS]
```

No subcommand-specific flags. Daemon-wide — `-d` is accepted but ignored.

```sh
tether --json list-devices
```

#### `ports`

List serial ports the daemon's host machine knows about (for picking a
device path). Returns an empty `ports` array on platforms/containers that
can't enumerate.

```
tether ports [OPTIONS]
```

No subcommand-specific flags.

```sh
tether --json ports
```

#### `config`

Show or change the live serial configuration. With no flags, prints current
settings. With one or more of the flags below, applies a partial update
(`set_device`) and prints the new state — the port stays open, in-flight
reads/writes aren't interrupted.

```
tether config [OPTIONS]
```

| Flag | Values | Meaning |
|---|---|---|
| `--baud <N>` | e.g. `9600`, `115200`, `921600` | New baud rate. |
| `--data-bits <N>` | `5`\|`6`\|`7`\|`8` | Data bits. |
| `--parity <P>` | `none`\|`odd`\|`even` | Parity. |
| `--stop-bits <N>` | `1`\|`2` | Stop bits. |
| `--flow <F>` | `none`\|`software`\|`hardware` | Flow control. |

```sh
tether -d board0 config                    # show
tether -d board0 config --baud 921600      # change live
```

A PTY-backed device (`pty=` on the daemon, or the client's own `tether pty`)
can't accept these — returns `-32007 unsupported_serial_op`.

#### `agents`

Print a self-contained cookbook for AI agents and exit — no daemon needed.
The canonical commands, exit codes, and pitfalls, safe to paste into an
`AGENTS.md` / `CLAUDE.md` block.

```
tether agents [OPTIONS]
```

No subcommand-specific flags.

```sh
tether agents > AGENTS.md
```

### Line control

Hardware-only — a PTY/pipe-backed device returns `-32007
unsupported_serial_op` for all four.

#### `break`

Send a serial BREAK pulse.

```
tether break [OPTIONS]
```

| Flag | Default | Meaning |
|---|---|---|
| `--duration-ms <MS>` | `250` | Break duration (matches `tio`'s default). |

#### `dtr`

Drive the DTR (Data Terminal Ready) output line.

```
tether dtr [OPTIONS] <STATE>
```

| Arg | Values | Meaning |
|---|---|---|
| `<STATE>` | `on`\|`off` | Assert / deassert. |

#### `rts`

Drive the RTS (Request To Send) output line. Same shape as `dtr`.

```
tether rts [OPTIONS] <STATE>
```

| Arg | Values | Meaning |
|---|---|---|
| `<STATE>` | `on`\|`off` | Assert / deassert. |

#### `lines`

Read the four input modem status lines.

```
tether lines [OPTIONS]
```

No subcommand-specific flags. Human output looks like `CTS=1 DSR=0 RI=0
DCD=1`; `--json` gives `{"cts":true,"dsr":false,"ri":false,"dcd":true}`.

### Virtual port

#### `pty`

Create an on-demand virtual serial port on **this** machine, bridged to the
device through the daemon (works over `-s tcp://...` too — a board on a lab
host becomes a local port on your laptop). Any serial tool (minicom, screen,
pyserial, a vendor flasher) can open the published path like a real device
node. Distinct from the daemon's own always-on `pty=` device option (see
`tetherd -D` below) — this one is client-side and needs no daemon
reconfiguration.

```
tether pty [OPTIONS] [-- <EXEC>...]
```

| Arg/Flag | Default | Meaning |
|---|---|---|
| `[EXEC]...` | — | Spawn this command once the port is ready; `{}` in its args is replaced with the link path (appended as a final arg if no `{}` appears). Bridging continues while it runs; `tether pty` exits with its status. |
| `--link <PATH>` | `/tmp/tether-<id>-<n>.pty` | Publish the slave under this path instead. |
| `--read-only` | off | Bytes typed into the virtual port are counted and dropped instead of reaching the device. |
| `--lock` | off | Hold the device's exclusive writer lock for the session (needs a `tetherd` new enough to know the `lock` RPC). |

Baud set by the attached tool is a no-op — a PTY has no UART; the real rate
is the daemon's device config. Run one `tether pty` per concurrent tool
(never point two tools at the same virtual port — the kernel splits the byte
stream between simultaneous readers). See [COOKBOOK.md](COOKBOOK.md) for the
full flashing recipe (`reset` + `pty --lock`).

```sh
tether -d board0 pty -- minicom -D {}
tether -d board0 pty --link /tmp/board0.pty
tether -d board0 pty --lock -- esptool --port {} --before no_reset write_flash ...
```

### Lifecycle

#### `reset`

Reset the board by driving a DTR/RTS sequence on the **real** port (not a
PTY — a virtual port carries no modem control, so this is how you reset a
board that's currently shared through `pty=`).

```
tether reset [OPTIONS]
```

| Flag | Default | Meaning |
|---|---|---|
| `--esp32` | — | esptool "classic" auto-reset preset (`dtr=0 rts=1 wait=100 dtr=1 rts=0 wait=50 dtr=0`) — leaves the chip in download mode. |
| `--seq <SEQ>` | — | Custom sequence: space/comma-separated `dtr=0\|1`, `rts=0\|1`, `wait=<ms>` steps, executed in order. |

Exactly one of `--esp32` / `--seq` is required. `--esp32` is shorthand for
`--seq "dtr=0 rts=1 wait=100 dtr=1 rts=0 wait=50 dtr=0"`.

```sh
tether -d esp32 reset --esp32
```

#### `reconnect`

Drop and reopen the serial device — for a wedged bus (`status` shows
`connected:true` but commands hang; usually a USB driver hiccup or a
half-completed board reset).

```
tether reconnect [OPTIONS]
```

| Flag | Default | Meaning |
|---|---|---|
| `--nowait` | off | Don't wait for the device to come back online. |
| `--timeout-ms <MS>` | `5000` | How long to wait for the reopen. |

#### `disconnect`

Explicitly close the port; auto-reconnect pauses until `connect`. Use this to
hand the device to another tool (`flashrom`, a vendor flasher) without
killing the daemon — the daemon remembers the explicit-disconnect state and
won't auto-reopen.

```
tether disconnect [OPTIONS]
```

No subcommand-specific flags.

#### `connect`

Reopen a port closed by `tether disconnect`. No-op if the device wasn't
explicitly disconnected.

```
tether connect [OPTIONS]
```

No subcommand-specific flags.

## `tetherd` reference

```
tetherd --device <[ID=]PATH[,KEY=VAL...]> [OPTIONS]
```

| Flag | Default | Meaning |
|---|---|---|
| `-D, --device <SPEC>` | — | Serial device to manage; repeatable for multiple devices. See spec table below. |
| `-b, --baud <BAUD>` | `115200` | Baud fallback for devices without an inline `baud=`. |
| `--data-bits <N>` | `8` | Data bits fallback: `5`\|`6`\|`7`\|`8`. |
| `--parity <P>` | `none` | Parity fallback: `none`\|`odd`\|`even`. |
| `--stop-bits <N>` | `1` | Stop bits fallback: `1`\|`2`. |
| `--flow-control <F>` | `none` | Flow control fallback: `none`\|`software`\|`hardware`. |
| `-s, --socket <PATH>` | `/tmp/tetherd.sock` | UDS listener path (or `/tmp/tetherd-<NAME>.sock` with `--name`). |
| `--name <NAME>` | — | Tag this daemon; defaults the UDS path to `/tmp/tetherd-<NAME>.sock`. Ignored if `--socket` is given explicitly. |
| `--no-uds` | off | Disable the UDS listener (TCP only — requires `--tcp`). |
| `--tcp [<HOST:PORT>]` | off | Also listen on TCP. Bare `--tcp` = `0.0.0.0:5557`. |
| `--auth-token <TOKEN>` | random, logged | Auth token for TCP clients. |
| `--buffer-capacity <BYTES>` | `65536` | Ring buffer size per device. |

### `-D` spec forms

Each `-D`/`--device` flag takes `[ID=]PATH[,KEY=VAL,...]`:

```
/dev/ttyUSB0                                  bare path — id derived from the basename
                                               (tty./cu. prefix stripped, e.g. ttyUSB0)
board0=/dev/ttyUSB0                           explicit id (recommended once you run >1 board)
board0=/dev/ttyUSB0,baud=921600,parity=odd    per-device inline overrides
```

Inline keys — all optional, falling back to the matching global flag when
omitted:

| Key | Values | Meaning |
|---|---|---|
| `baud` | integer | Baud rate for this device. |
| `data-bits` | `5`\|`6`\|`7`\|`8` | Data bits. |
| `parity` | `none`\|`odd`\|`even` | Parity. |
| `stop-bits` | `1`\|`2` | Stop bits. |
| `flow` | `none`\|`software`\|`hardware` | Flow control. |
| `pty` / `pty=<PATH>` | — | Also expose a daemon-side virtual serial port for a non-tether tool. Bare `pty` links at `/tmp/tether-<id>.pty`; `pty=<PATH>` uses your own path. A PTY has no modem lines or real baud — DTR/RTS auto-reset and mid-stream baud changes aren't forwarded through it. |
| `shell` | `posix`\|`uboot`\|`none` (default `posix`) | Console personality. `uboot` forces CR-only framing for `exec`/`run` and defaults `newline=cr`; `none` makes `exec` refuse immediately (raw console — use `run`/`send`/`expect`). |
| `prompt` | regex | Default `-u` for `run`/`sync`. Must not contain a comma (the spec is comma-delimited). |
| `newline` | `lf`\|`cr`\|`crlf`\|`none` | Default line terminator for `run`/`exec`. |

These settings are reported back in `tether list-devices` / `tether status`
(`shell`, `prompt`, `newline` fields).

```sh
tetherd -D /dev/ttyUSB0                                        # single device, defaults
tetherd -D /dev/ttyUSB0 --tcp                                  # expose over TCP too
tetherd -D 'board0=/dev/ttyUSB0' -D 'board1=/dev/ttyUSB1,baud=9600'   # multi-device
tetherd -D 'board=/dev/ttyUSB0,shell=uboot,prompt==> ',newline=cr'    # console personality
```

## Exit codes

| Code | Meaning |
|---|---|
| `0` | ok / matched |
| `1` | usage error (clap — bad flags/args) |
| `2` | protocol/RPC error (any JSON-RPC error not covered by a code below, e.g. `-32015 ambiguous_device`, `-32009 device_not_found`) |
| `3` | connection error (couldn't dial the socket — daemon down, wrong `-s`, etc.) |
| `4` | device disconnected (USB unplugged, or the device is explicitly `disconnect`ed) |
| `5` | buffer overflow (no match within the 1 MiB search window; not related to `--max-output-bytes`, which only truncates) |
| `6` | lock contention (another session holds the writer lock; a one-line explanation goes to stderr) |
| `7` | unauthorized (TCP transport: `--auth-token`/`TETHER_AUTH_TOKEN` missing or wrong) |
| `8` | `exec` ran, but the device shell reported no numeric status (non-POSIX console; `exit_code: null` in `--json`) — see [EXEC_NONPOSIX_SHELLS.md](EXEC_NONPOSIX_SHELLS.md) |
| `124` | timeout (no match within `--timeout-ms`) |

Two commands additionally **pass through a child/device exit status** instead
of using the table above:

- `exec` exits with the **device command's own status** (like `ssh`) — a
  non-zero exit usually means your device command failed, not `tether`.
- `pty -- <EXEC>...` exits with `<EXEC>`'s own status once it exits (a
  signal death is reported as `1`).

Note exit `6` (lock contention) prints **no message at all**, on either
stdout or stderr, even with `--json` — the exit code is the only signal.
Check `tether status` → `lock.holder_session_id` to see who holds it.

## Environment variables

| Variable | Effect |
|---|---|
| `TETHER_NONINTERACTIVE=1` | Same as `--no-interactive`: never show an interactive picker; agents/automation under a PTY should always set this. |
| `TETHER_AUTH_TOKEN` | Same as `--auth-token` (client-side, for TCP transport). |
| `TETHER_PTY` | Set by `tether pty -- <EXEC>...` in the spawned child's environment to the virtual port's path (in addition to `{}` substitution in its args). |
| `RUST_LOG` | Log level for `tetherd`'s tracing output (e.g. `RUST_LOG=debug`). Defaults to `info`. Read by the daemon only. |

There is no `tetherd`-side equivalent of `TETHER_AUTH_TOKEN` — pass
`--auth-token` explicitly or let the daemon generate and print one.
