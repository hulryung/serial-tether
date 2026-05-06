# Serial Tether

![Serial Tether — share any serial port, anywhere](assets/banner.png)

> A daemon and CLI that lets AI agents and humans share a single serial device.

**Site**: <https://hulryung.github.io/serial-tether/> · **Demo**: <https://hulryung.github.io/serial-tether/#demo>

**Docs**:
- [`docs/OVERVIEW.md`](docs/OVERVIEW.md) — what it is, why, and how it works (read this first)
- [`docs/AI_AGENT_GUIDE.md`](docs/AI_AGENT_GUIDE.md) — **how to teach Claude Code / Codex / Cursor to use it** (paste-and-go AGENTS.md block + verification script)
- [`docs/AGENT_USAGE.md`](docs/AGENT_USAGE.md) — one-page command cookbook the AI agent itself reads
- [`docs/PROTOCOL.md`](docs/PROTOCOL.md) — JSON-RPC 2.0 / NDJSON wire spec (v1)
- [`examples/`](examples/) — five working Bash automation scripts

`tetherd` owns the serial port; multiple clients (`tether`, user scripts, the future `tether-tui`) connect over a Unix socket / Named Pipe / TCP and read and write concurrently. The agent-facing CLI (`tether`) is transactional, structured, and bounded by design: JSON-RPC responses carry decoded text, exit codes follow shell conventions, the `run` primitive is race-free at the daemon level, and output truncation guards LLM context budgets.

## Why

Embedded development is a lot of staring at a serial console — kicking a
bootloader, reading kernel logs, exercising firmware against a corner case.
Increasingly an AI coding agent wants to do that staring too: react to a
stack trace, set a U-Boot env var, drive a board through a regression suite,
read sensor output, retry after a flash. The naïve loop — "agent describes
a command in chat → human copies it into `tio` → human pastes the output
back" — is slow, brittle, and pointless.

Serial Tether's job is to hand that loop directly to the agent **without
elbowing the human out**. The daemon takes ownership of `/dev/ttyUSB0`
once. From there, three audiences share the same port at the same time:

- **AI agents** drive the board through a JSON-RPC CLI that is transactional,
  structured, and bounded by design. Race-free `run`, ANSI-stripped and
  echo-stripped `output`, standard exit codes, configurable length
  truncation so the LLM context never blows up — the things that turn flaky
  scripted automation into reliable scripted automation.
- **Humans** stay in full control on the same port: drop into a `tio`-style
  raw-mode interactive shell with `tether`, tail every byte the agent is
  sending and receiving with `tether tail`, override or interrupt at will.
  No "agent mode" that locks the operator out — quite the opposite, the
  human gets a god's-eye view of what every other client is doing.
- **CI and shell scripts** ride the same wire. `if tether run … ; then …;
  case $? in 124) … esac` is just a few lines, and it works the same way
  whether the daemon is on this machine, behind SSH, or on a VM across the
  room.

The whole thing is meant to be agnostic about *what* sits on the other end
of the serial link — U-Boot, Linux console, busybox login, vendor monitors,
RTOS REPLs, raw MCU debug streams — because the daemon just shuttles bytes
and surfaces them with race-free framing. Intelligence about prompts,
escape sequences, and command grammars belongs in the client (or the agent
driving it), where it's easy to evolve.

In one line: **modern, AI-friendly, multi-tenant access to the serial port
across the whole spectrum of embedded development, without taking the port
away from the engineers who have always lived inside it.**

![Serial Tether usage model — same serial port, shared by humans and AI in real time](assets/usage-model.png)

## Demo

![Two interactive `tether` shells sharing one serial console](assets/dual_shell_demo.gif)

Two humans, one serial console — every byte the device emits is broadcast
to every attached session. Think `screen -x` for a U-Boot prompt.

<details>
<summary><b>More flavors</b> — same idea, different right-pane (click to expand)</summary>

### CLI + shell

Human in a `tether` shell on the left; on the right, a scripter
running one-shot `tether run` / `ports` / `config` from another terminal.
The scripter's commands echo live into the human's pane.

![cli + shell](assets/cli_demo.gif)

### Agent + shell

Same setup, but the right pane is an LLM/agent calling `tether --json run`
for transactional RPCs (with a live `tether config --baud` toward the end).

![agent + shell](assets/split_demo.gif)

</details>

> Higher-fidelity (asciinema-player, click to seek between the three):
> <https://hulryung.github.io/serial-tether/#demo>

## Components

The `serial-tether` package ships two binaries:

- **`tetherd`** — daemon. Owns the serial port; fans a single ring buffer out to every attached session.
- **`tether`** — non-interactive CLI. `send` / `expect` / `run` / `status` / `tail` / `sync`.

Plus a supporting library:

- **`tether-protocol`** — wire-protocol types and NDJSON codec (shared between daemon and client).
- **`tether-tui`** *(planned)* — interactive TUI client for human use.

## Install (macOS / Linux)

Every option below installs both `tetherd` and `tether`. Pick the one you prefer.

**Homebrew** (recommended on macOS — no Rust toolchain needed):
```sh
brew install hulryung/tether/serial-tether
```

**`cargo install`** (with a Rust toolchain — works on any platform Rust supports):
```sh
cargo install serial-tether
```

**Pre-built binaries via curl** (no dependencies):
```sh
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/hulryung/serial-tether/releases/download/v0.8.0/serial-tether-installer.sh | sh
```

Or **build from source**:
```sh
git clone https://github.com/hulryung/serial-tether
cd serial-tether
cargo build --workspace --release
# binaries land in ./target/release/{tetherd,tether}
```

## Quick start

```sh
# Standalone — `tether` brings up its own private daemon for one device,
# then shuts it down when you exit. Same UX as `tio /dev/ttyUSB0`:
tether -D /dev/tty.usbserial-XXXX -b 115200      # Ctrl-A then Q to quit

# Multi-client — start a long-lived daemon, attach as many clients as you
# want from any terminal (or remote, with --tcp).
# Terminal 1:
tetherd -D /dev/tty.usbserial-XXXX -b 115200
# Terminal 2 — interactive shell:
tether
# Terminal 3 — agent / scripted:
tether status
tether run "version" -u "# " --literal --timeout-ms 3000 --json
tether tail
```

If the daemon isn't running, `tether` prints exactly how to start it.

### Remote daemon (TCP)

To drive a board attached to one host from another machine, start the daemon
with TCP listening:

```sh
# On the daemon host:
tetherd -D /dev/tty.usbserial-XXXX -b 115200 --tcp
# Banner prints the auto-generated token and every reachable IP. Pin the
# token explicitly with --auth-token MYSECRET if you want it stable across
# restarts. Use --tcp 127.0.0.1:5557 for loopback only.

# On the agent host:
TETHER_AUTH_TOKEN=MYSECRET tether -s tcp://daemon-host:5557 status
TETHER_AUTH_TOKEN=MYSECRET tether -s tcp://daemon-host:5557 run "version" \
    --newline crlf -u "# " --literal --timeout-ms 3000 --json
```

UDS connections are authenticated by the OS (file permissions); TCP
connections always require a token. Run with both `-s /tmp/tetherd.sock` and
`--tcp ...` to expose the daemon on both transports simultaneously.

### More than one board

There are two ways to run several boards. Pick whichever suits the setup.

**One daemon per board** (process-level isolation, simplest):

```sh
tetherd -D /dev/tty.usbserial-A --name board0    # in one terminal
tetherd -D /dev/tty.usbserial-B --name board1    # in another

tether --name board0 status                       # talk to board0
tether --name board1 run "version" -u "# " --literal --timeout-ms 3000
```

`--name` defaults each daemon's UDS to `/tmp/tetherd-<NAME>.sock` so
they don't collide.

**One daemon, multiple devices** (single endpoint, single auth token —
since v0.8):

```sh
# Repeat -D / --device for each port. Each spec is `[id=]path[,key=value...]`.
tetherd \
  -D 'board0=/dev/tty.usbserial-A' \
  -D 'board1=/dev/tty.usbserial-B,baud=921600,parity=odd'

# Clients address devices by id with -d / --device.
tether -d board0 status
tether -d board1 run "version" -u "# " --literal --timeout-ms 3000
tether list-devices
```

Per-device options inside `-D`: `baud`, `data-bits`, `parity`, `stop-bits`,
`flow`. Anything omitted falls through to the global `--baud` / `--parity` /
etc. flags. If a client omits `--device` against a multi-device daemon
the call returns `-32015 ambiguous_device` — pick a specific one.

The plain `tetherd -D /dev/ttyX` and `tether <cmd>` (no flags) still use
`/tmp/tetherd.sock` and the only device — single-board setups don't change.

## The one command an agent should reach for

```sh
tether --json run "$cmd" -u "$prompt" --literal --timeout-ms 5000
# → { matched, match, output (decoded text), truncated, duration_ms, ... }
# → exit 0 (ok) / 124 (timeout) / 2 (protocol) / 3 (connect) / 4 (device) / 5 (overflow) / 6 (lock) / 7 (unauthorized)
```

Agent-friendly defaults are baked in: `--strip-ansi`, `--strip-echo`, `--max-output-bytes 8192`. The `--json` payload includes a decoded `output` field so an LLM never has to deal with base64.

### Pointing an AI agent at your board

If you want **Claude Code / Codex / Cursor** to drive the console, see
[`docs/AI_AGENT_GUIDE.md`](docs/AI_AGENT_GUIDE.md) — a paste-and-go
`AGENTS.md` / `CLAUDE.md` block plus a four-step verification script you
can ask the agent to run before handing it the actual task.

## Protocol

[`docs/PROTOCOL.md`](docs/PROTOCOL.md) — JSON-RPC 2.0 over NDJSON. The same wire format works on UDS, Named Pipe, or TCP.

## Verifying the build

A virtual serial pair smoke test (no `socat` required, only Python 3 and the built binaries):

```sh
bash tools/smoke_test.sh
```

## Status

Shipped through v0.8.0:
- ✅ Multi-device daemon: one `tetherd` owns N ports, addressed by `--device <id>` (v0.8.0)
- ✅ `list_devices` RPC + `tether list-devices` CLI (v0.8.0)
- ✅ Per-device startup config: `-D 'id=path,baud=N,parity=...'` (v0.8.0)
- ✅ Tio-style line / break / modem control: `send_break` / `set_dtr` / `set_rts` / `read_modem_status` + shell escapes Ctrl-A B/D/R/L (v0.8.0)
- ✅ Operator-driven port hold: `disconnect_device` / `connect_device` (v0.8.0)

Through v0.7.x:
- ✅ `list_ports` / `set_device` / live `tether config --baud` etc. (v0.7.0)
- ✅ Shell escapes Ctrl-A C (config) / V (ports) (v0.7.0)
- ✅ `--name <NAME>` for running multiple daemons side-by-side (v0.7.1)

Through v0.6.0:
- ✅ `hello` / `attach` / `detach` / `send` / `expect` / `run` / `status`
- ✅ writer lock with `preempt` policy (queue / fail / force)
- ✅ `strip_ansi` / `strip_echo` / `max_output_bytes` (with truncation marker)
- ✅ standard exit codes; decoded `output` field in `--json`
- ✅ `sync` (send CR, wait until idle, surface a prompt candidate)
- ✅ ring-buffer fan-out with separate consumer / notify cursors per session
- ✅ TCP transport with token auth (`--tcp [HOST:PORT] --auth-token …`)
- ✅ Single daemon can listen on UDS and TCP simultaneously
- ✅ Startup banner enumerating reachable IPs and the auth token
- ✅ `tether shell` — interactive raw-mode client (Ctrl-A then Q to quit)
- ✅ `tether` (no subcommand) drops into the shell
- ✅ Friendly error when the daemon isn't running (with the command to start one)
- ✅ Auto-reconnect on the daemon side when the device disappears (USB unplug, etc.)
- ✅ `tether reconnect` RPC + `--auto-reconnect` client flag for retry-on-disconnect
- ✅ `device` notifications (disconnected / reconnected) shown in `tail` and `shell`
- ✅ Standalone mode: `tether -D /dev/ttyUSB0` auto-spawns a private daemon

Not yet:
- Windows Named Pipe backend
- 30-second session resume after disconnect
- `cancel` method
- TLS for TCP (use SSH/WireGuard for untrusted networks for now)

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or <http://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or <https://opensource.org/license/mit>)

at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in the work by you, as defined in the Apache-2.0 license, shall
be dual licensed as above, without any additional terms or conditions.
