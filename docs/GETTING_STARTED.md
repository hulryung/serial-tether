# Getting Started

You just plugged in a USB-serial adapter and want to talk to whatever's on
the other end — U-Boot, a Linux console, a vendor monitor — without losing
access to it every time you switch tools. This gets you from nothing to
productive in about 10 minutes.

## What tether is

`tetherd` is a daemon that opens the serial port once and owns it.
`tether` is the client: every invocation — an interactive shell, a one-shot
command, a log tail — attaches to the daemon over a socket instead of
touching the device directly. Any number of clients can attach at the same
time, so a human, a script, and an AI agent can drive (or just watch) the
same board concurrently.

```
                    /dev/ttyUSB0 (or /dev/cu.usbserial-XXXX)
                            │
                       tetherd  (owns the port, holds a ring buffer)
                            │
              ┌─────────────┼─────────────┐
              │             │             │
         tether shell   tether tail   tether exec
          (you, typing)  (watching)   (script / agent)
```

Everything below works the same on macOS and Linux; examples use
`/dev/ttyUSB0` — swap in your own path (`/dev/cu.usbserial-XXXX` on macOS).

## Install

Pick one:

```sh
# Homebrew (macOS, no Rust toolchain needed)
brew install hulryung/tether/serial-tether

# cargo (any platform Rust supports)
cargo install serial-tether

# Pre-built binaries via curl (no dependencies)
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/hulryung/serial-tether/releases/download/v0.9.3/serial-tether-installer.sh | sh
```

All three install both `tetherd` and `tether`. Confirm with `tether --version`.

## First contact — zero config

You don't need to know anything about daemons yet. Just run:

```sh
tether
```

With no daemon running and no path given, `tether` looks for USB serial
adapters and — if you're at an interactive terminal — lets you pick one (or
picks the only one automatically), spins up a private daemon behind the
scenes, and drops you into an interactive shell. Quit with `Ctrl-A` then
`Q`, and the private daemon goes with it. Same idea as `tio` or `picocom`.

If you already know the path, skip the picker:

```sh
tether /dev/ttyUSB0                   # tio-style: auto-daemon + shell
tether -b 9600 -D /dev/ttyUSB0        # same, with an explicit baud
```

> **macOS: use `cu.*`, not `tty.*`.** Every USB-serial adapter shows up
> twice — `/dev/cu.usbserial-XXXX` (dial-out, what you want) and
> `/dev/tty.usbserial-XXXX` (dial-in, waits for carrier detect and can hang
> opens). `tether`'s own picker already filters the `tty.*` duplicate out;
> if you're typing a path by hand, use `cu.*`.

This is fine for a quick poke. For anything you'll come back to — multiple
terminals, a script running alongside your interactive session, an agent —
set up the long-lived daemon below instead.

## The long-lived daemon (recommended setup)

Start `tetherd` once, in its own terminal, and leave it running:

```sh
tetherd -D /dev/ttyUSB0 -b 115200
```

```
Serial Tether 0.10.0
  device     /dev/ttyUSB0 @ 115200 baud
  framing    8N1 flow=none
  buffer     64 KiB

Listening:
  unix       /tmp/tetherd.sock

  try:       tether status
```

Now attach as many clients as you like, from any number of terminals —
none of them compete for the port, and none of them have to know about the
others:

```sh
tether status                          # daemon + device info
tether list-devices                    # ids managed by this daemon
tether shell                           # interactive, in one terminal
tether tail                            # read-only log, in another
tether exec "uname -a"                 # one-shot, from a script or agent
```

Why bother with a separate daemon instead of always using the zero-config
form? Because the zero-config daemon dies with the client that spawned it —
fine for a quick poke, useless for "leave a log running while I also run a
flash script and let an agent poke at it." The long-lived daemon is what
makes all three coexist.

## The four everyday commands

### `shell` — interactive, for you

```sh
tether shell
```

```
[tether shell — Ctrl-A Q quit, Ctrl-A B/D/R/L (break/dtr/rts/lines), Ctrl-A C/V (config/ports), Ctrl-A ? help]
```

Keystrokes go to the device, device output renders live. `Ctrl-A` is the
escape prefix:

| Keys | Does |
|---|---|
| `Ctrl-A Q` | quit |
| `Ctrl-A ?` | show this help overlay |
| `Ctrl-A C` | show live serial config |
| `Ctrl-A V` | list available serial ports |
| `Ctrl-A B` / `D` / `R` / `L` | BREAK / toggle DTR / toggle RTS / modem status |
| `Ctrl-A Ctrl-A` | send a literal Ctrl-A to the device |

Plain `tether` with no daemon running and no subcommand is shorthand for
this same shell (see "zero config" above).

### `tail` — watch, don't touch

```sh
tether tail                            # only new output from here on
tether tail --from start               # replay the buffer, then follow
tether tail > logs/console.log         # data → stdout, clean log on disk
```

`tail` is read-only: it doesn't take the writer lock, so it runs alongside
a `shell`, an `exec`, or another `tail` without interfering. Device bytes go
to stdout; connect/disconnect notifications go to stderr — redirect stdout
alone for a clean log:

```sh
tether tail 2>/dev/null > logs/console.log   # bytes only, no event chatter
```

### `exec` — run a command, get its output and exit code

For a device sitting at a shell prompt (busybox, dash, bash, U-Boot hush),
`exec` is the 90% case: no prompt regex to guess, no BEGIN/END scaffolding
to hand-roll.

```sh
tether exec "uname -a"                 # output → stdout
echo "exit: $?"                        # → the *device* command's exit code, ssh-style

tether exec "test -f /etc/os-release"
echo "exit: $?"                        # 0 or 1 from the device, not from tether

tether exec "cat /proc/uptime" --json
# {"output": "1234.56 5678.90\n", "exit_code": 0, "duration_ms": 12, "truncated": false}
```

Composes like any shell command:

```sh
if tether exec "grep -q ok /tmp/state"; then echo "ready"; fi
```

**Footgun:** `exec` defaults to a `cr` line terminator, which suits both
Linux shells and U-Boot. If you pass `--newline crlf` toward a U-Boot
console, it runs every command **twice** — CR executes the line, the
trailing LF arrives as an empty line, and U-Boot's CLI repeats the last
command on it. Register the device once (`shell=uboot` — see
[CLI_REFERENCE.md](CLI_REFERENCE.md)) and this is handled for you; otherwise
just don't pass `crlf` toward a bootloader.

### `config` — view or change the live serial settings

```sh
tether config
# path:         /dev/ttyUSB0
# baud:         115200
# framing:      8N1
# flow_control: none
# connected:    true
# shell:        posix

tether config --baud 9600              # applied live, no reconnect needed
```

**Footgun:** this changes the *daemon's* view of the real port. It has no
effect on a client-side virtual port (`tether pty`, below) — a baud rate a
flashing tool sets there is silently a no-op, because a PTY has no UART.
Change the real baud with `tether config --baud` instead.

## A taste of the advanced stuff

Three things you'll eventually want, each covered in full in
[COOKBOOK.md](COOKBOOK.md) / [CLI_REFERENCE.md](CLI_REFERENCE.md):

**Let another tool share the port** — `tether pty` opens a client-side
virtual serial port bridged to the device, so minicom/pyserial/a flasher can
use it like a real device node while `tetherd` still owns the real one:

```sh
tether pty -- minicom -D {}            # {} = the virtual port path
```

**Drive a board from another machine** — start the daemon with `--tcp`,
then point a remote client at it with a token:

```sh
tetherd -D /dev/ttyUSB0 --tcp                             # banner prints the token
TETHER_AUTH_TOKEN=<token> tether -s tcp://host:5557 status
```

**Set up an AI agent** — run `tether agents` for a self-contained cookbook
(exit codes, JSON shapes, common pitfalls) you can paste straight into an
`AGENTS.md` / `CLAUDE.md` file:

```sh
tether agents
```

## Where to next

| I want to... | Read |
|---|---|
| Do a specific task (flash a board, watch a boot log, run CI) | [COOKBOOK.md](COOKBOOK.md) |
| Look up an exact flag or subcommand | [CLI_REFERENCE.md](CLI_REFERENCE.md) |
| Fix an error I'm seeing | [TROUBLESHOOTING.md](TROUBLESHOOTING.md) |
| Understand the daemon/client architecture | [OVERVIEW.md](OVERVIEW.md) |
| Set up an AI agent | [AGENT_USAGE.md](AGENT_USAGE.md), or just run `tether agents` |
| Integrate programmatically / speak the wire protocol | [PROTOCOL.md](PROTOCOL.md) |
| Run `exec` against U-Boot or another non-POSIX shell | [EXEC_NONPOSIX_SHELLS.md](EXEC_NONPOSIX_SHELLS.md) |

## Stopping and cleaning up

- Interactive shell: `Ctrl-A` then `Q`.
- Kill a long-lived daemon: `pkill -f tetherd` (or `Ctrl-C` the terminal
  it's running in).
- **The daemon outlives the terminals that attached to it — on purpose.**
  Closing every `tether shell` / `tail` / `exec` you had open does *not*
  stop `tetherd`; that's the entire point of running one (buffered output
  survives, other clients keep working, an agent's session isn't tied to
  your terminal). Only the zero-config form (plain `tether` or
  `tether <PATH>` with no daemon already running) ties the daemon's life
  to that one client and cleans up automatically on exit.
