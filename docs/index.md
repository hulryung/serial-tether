---
layout: home

hero:
  name: Serial Tether
  text: One serial port. One daemon. Many clients.
  tagline: A daemon and CLI that lets AI agents and humans share a single serial device. Race-free RPC, raw-mode shell, UDS or TCP, cross-platform.
  actions:
    - theme: brand
      text: Get Started
      link: /GETTING_STARTED
    - theme: alt
      text: Cookbook
      link: /COOKBOOK
    - theme: alt
      text: View on GitHub
      link: https://github.com/hulryung/serial-tether

features:
  - title: One port, many clients
    details: A human's raw-mode shell, an AI agent's JSON-RPC, and a logger's tail all attach to the same /dev/ttyUSB0 at once. No "agent mode" that locks the operator out.
  - title: exec with real exit codes
    details: Shell-friendly exit codes (124 timeout, 7 unauthorized, 4 device gone, …) so "if tether exec …; then" just works in scripts and CI.
  - title: Virtual serial ports
    details: "tether pty bridges a client-side PTY to the shared port — hand any flasher its own /dev/pts device, port lives exactly as long as the tool does."
  - title: Remote by default
    details: Native TCP transport with token auth, identical wire format to the local Unix socket — same commands whether the daemon is local or across the lab.
  - title: Race-free run
    details: run takes the writer lock, marks the ring-buffer head before writing, and matches from there. No client-side send-then-expect guessing.
  - title: Zero-config start
    details: tether with no daemon running tells you exactly how to start one, and tether reset recovers a wedged shared port without unplugging anything.
---

## Why

Embedded development means a lot of staring at a serial console — kicking a
bootloader, reading kernel logs, exercising firmware against a corner case.
Increasingly an AI coding agent wants to do that staring too: react to a
stack trace, set a U-Boot env var, drive a board through a regression suite.
The naïve loop — *agent describes a command in chat → human copies it into
`tio` → human pastes the output back* — is slow, brittle, and pointless.

Serial Tether's job is to hand that loop directly to the agent **without
elbowing the human out**. The daemon takes ownership of `/dev/ttyUSB0` once;
from there, AI agents, humans, and scripts/CI all share the same port at the
same time — see [OVERVIEW](/OVERVIEW) for the full mental model.

## Install

All four paths give you the same two binaries: `tetherd` (daemon) and
`tether` (client).

::: code-group

```sh [Homebrew]
brew install hulryung/tether/serial-tether
```

```sh [cargo]
cargo install serial-tether
```

```sh [curl]
curl -fsSL https://github.com/hulryung/serial-tether/releases/latest/download/serial-tether-installer.sh | sh
```

```sh [From source]
git clone https://github.com/hulryung/serial-tether
cd serial-tether
cargo build --workspace --release
```

:::

```sh
# Terminal 1 — daemon owns the serial port
tetherd -D /dev/tty.usbserial-XXXX -b 115200 --tcp
# banner prints reachable IPs and an auto-generated auth token

# Terminal 2 — drop into a tio-style interactive shell
tether
# Ctrl-A then Q to quit

# Or, for an AI agent / shell script:
tether --json run "version" --newline crlf -u "ASAD SOC => " --literal --timeout-ms 3000

# Or, from a remote host (lima VM, CI runner, your laptop):
TETHER_AUTH_TOKEN=<token-from-banner> \
  tether -s tcp://daemon-host:5557 status
```

If the daemon isn't running, `tether` tells you exactly how to start one —
see the [Getting Started](/GETTING_STARTED) guide for the full walkthrough.

## Demo

Two humans in their own `tether` shells, attached to the same daemon on a
single serial port. Every byte the device emits is broadcast to every
session — `screen -x` for a U-Boot prompt.

<AsciinemaPlayer src="/dual_shell_demo.cast" auto-play />

<details>
<summary>More flavors — same idea, different right-pane</summary>

### CLI + shell

Human in a `tether` shell on the left; on the right, a scripter running
one-shot `tether run` / `ports` / `config` from another terminal. The
scripter's commands echo live into the human's pane.

<AsciinemaPlayer src="/cli_demo.cast" />

### Agent + shell

Same setup, but the right pane is an LLM/agent calling `tether --json run`
for transactional RPCs, plus a live `tether config --baud` toward the end.

<AsciinemaPlayer src="/split_demo.cast" />

</details>

<details>
<summary>Multiple boards on one daemon (v0.8) — different concept</summary>

One `tetherd` process owns N serial ports. Clients address each by an
operator-chosen id (`tether -d board0`, `tether -d board1`), with per-device
baud / parity / etc. inline in `-D`. Each device has its own ring buffer,
writer lock, and event broadcast — traffic stays isolated.

<AsciinemaPlayer src="/multi_device_demo.cast" />

</details>
