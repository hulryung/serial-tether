# Serial Tether — Overview

What it is, why it exists, how to think about it. Read this once before
diving into [`AGENT_USAGE.md`](AGENT_USAGE.md) (commands cookbook) or
[`PROTOCOL.md`](PROTOCOL.md) (wire spec).

## The problem

A serial port is a precious singleton. The board on the other end of
`/dev/tty.usbserial-XXXX` accepts exactly one program at a time talking to
it — open it from `tio`, and your AI agent can't drive it; let the agent
have it, and you can't watch what's happening; close one, and you've lost
your bootloader prompt to a scrolled-off line.

Industrial console servers (`conserver`, `ser2net`) have solved this for
decades, but their UX targets sysadmins, not modern automation: no
structured output, no race-free request/response, no agent-friendly
defaults, no easy install.

Serial Tether is `conserver` rewritten for the era where a human, an AI
agent, and a CI script all want to drive the same dev board at the same
time, without stepping on each other.

## The architecture

Two binaries plus a wire protocol:

```
┌─────────────────────────────────────────────────────────────────┐
│                    /dev/tty.usbserial-XXXX                      │
│                            │                                    │
│                       termios @ 115200                           │
│                            ▼                                    │
│   ┌────────────────────────────────────────────────┐            │
│   │             tetherd  (the daemon)              │            │
│   │   ┌──────────────────────────────────────┐     │            │
│   │   │  Single ring buffer (64 KiB default) │     │            │
│   │   └────┬──────────┬─────────────┬────────┘     │            │
│   │        │ fan-out  │             │              │            │
│   │   ┌────▼────┐ ┌───▼────┐  ┌─────▼─────┐        │            │
│   │   │session A│ │session B│ │ session C │        │            │
│   │   └─────────┘ └─────────┘ └───────────┘        │            │
│   └─────┬──────────┬─────────────┬─────────────────┘            │
│         │          │             │                              │
│  ┌──────▼──┐  ┌────▼────┐  ┌─────▼─────┐                        │
│  │  UDS    │  │  UDS    │  │   TCP     │                        │
│  │  /tmp/. │  │  /tmp/. │  │  :5557    │                        │
│  └────┬────┘  └────┬────┘  └─────┬─────┘                        │
└───────┼────────────┼─────────────┼──────────────────────────────┘
        │            │             │
   ┌────▼────┐  ┌────▼────┐  ┌─────▼─────┐
   │ tether  │  │ tether  │  │  tether   │
   │  tail   │  │  shell  │  │   run     │
   │ (logs)  │  │ (human) │  │ (agent)   │
   └─────────┘  └─────────┘  └───────────┘
```

- **`tetherd`** — daemon. Holds the serial port open. Reads bytes into a
  shared ring buffer. Listens for clients on a Unix-domain socket and,
  optionally, on TCP. Schedules writes. Enforces an exclusive writer-lock
  during atomic command-and-response transactions.

- **`tether`** — the client. Each invocation opens one connection,
  attaches a session, does its work, and disconnects. Many `tether`
  processes can attach to the same daemon at the same time.

- **wire protocol** — JSON-RPC 2.0 framed as line-delimited JSON. Same
  shape over UDS and TCP. Plain text for debuggability; the daemon's
  `--log-protocol` dumps every message as NDJSON.

## The mental model

Think of `tetherd` as a screen-sharing app for one serial port:

- **The port is shared, not owned.** Whoever connects first does *not*
  block other clients. The kernel sees only `tetherd`; everyone else sees
  `tetherd`.

- **Output is broadcast to every viewer.** Bytes the device emits go into
  one ring buffer. Every attached session has its own *cursor* into that
  buffer. The daemon fans new bytes out to each session at the speed that
  session can keep up.

- **Input is cooperative, not exclusive.** Multiple clients *can* write
  to the device, and bytes interleave on the wire (just like multiple
  people typing on the same screen). For atomic command-and-response
  flows the client uses `run`, which acquires a writer-lock for the
  duration of the transaction so an agent and a human don't corrupt each
  other's output.

- **A session is light-weight.** It's just an entry in the daemon's
  table: an identifier, a couple of cursors, a mode (`rw` or `ro`), and
  a flow-control policy. Attaching costs one round-trip; you can hold
  many concurrently.

If you've used SSH multiplexing or `tmate`, the model is familiar.

## Two cursors per session

Most ring-buffer designs have one read pointer per consumer. Serial
Tether has two:

- **`consumer_cursor`** — what `expect`/`run` matches against. Advances
  only when the session's RPC call consumes data into a response.
- **`notify_cursor`** — what `data` push notifications stream to the
  client. Advances as bytes go out the wire.

This separation is what lets a `tether tail` (read-only) and a `tether
run` (atomic command) coexist on the same session: the tail consumer
doesn't drain bytes that the run is about to match against, and vice
versa. Without it you'd race: the daemon would either short-circuit
matches or stop streaming during a `run`.

## Three deployment shapes

### a) Single-machine, single user

```
tetherd -D /dev/ttyUSB0 -b 115200          # daemon
tether                                      # interactive shell, same machine
```

UDS socket (`/tmp/tetherd.sock`) authenticated by file permissions, no
network exposure. Same UX as `tio`/`screen`/`picocom` — you type, you
see — except now you can also fire `tether run …` from another terminal
without disconnecting your interactive session.

### b) Single-machine, mixed human + agent

```
# Terminal A — daemon
tetherd -D /dev/ttyUSB0 -b 115200

# Terminal B — you, watching
tether tail | tee logs/$(date +%F).log

# Terminal C — your shell for ad-hoc poking
tether shell

# AI agent (Claude Code, Codex, etc.) running concurrently:
tether --json run "version" --newline crlf -u "ASAD SOC => " --literal
```

Three sessions on one daemon. The agent's `run` blocks the writer for
~50 ms while it sends and matches; your typing in Terminal C queues
behind. Tail in Terminal B sees everything as it happens.

### c) Remote daemon

```
# Daemon host
tetherd -D /dev/ttyUSB0 --tcp                # banner prints token + IPs

# Anywhere else (your laptop, a VM, CI)
TETHER_AUTH_TOKEN=<from-banner> tether \
    -s tcp://daemon-host:5557

# … or for an agent over the same path
TETHER_AUTH_TOKEN=<…> tether -s tcp://… --json run …
```

UDS authentication-by-permissions doesn't extend across machines, so
TCP requires a token. The daemon mints one at startup if you don't
provide `--auth-token`. The same wire protocol runs unchanged over TCP.

## Three commands you'll reach for, and why

| Command | When | What it really does |
|---|---|---|
| `tether` (no args) | "I just want to type at the board." | Drops into raw-mode interactive shell. Each keystroke goes through `send`; `data` notifications stream to your stdout. Ctrl-A then Q exits. |
| `tether tail` | "Watch what's happening, don't touch." | Read-only attach. Streams `data` notifications to stdout. Useful for logging, observation, debugging an agent's session. |
| `tether run "<cmd>" -u "<prompt>" --literal` | "Send this command, capture the response." | Atomic transaction at the daemon: writer-lock held, captured `head_seq` before the write, matched after. Idempotent and race-free, which matters when an agent fires hundreds of these. |

Three lower-level commands sit underneath:

- `tether send <bytes>` — write only, no waiting. Useful for one-shot
  control sequences (`Ctrl-C` to break a hung command, etc.).
- `tether expect <pattern>` — wait only, no send. Rarely used directly;
  `run` is almost always what you want.
- `tether sync` — send a `\r`, wait for the device to fall idle, return
  the last non-empty line. Used by an agent at session start to discover
  the prompt without hard-coding it.

## Why these specific design choices

- **Daemon, not a library.** Multiple processes on the same machine
  can't share a serial port at the kernel level. Pushing the multiplexing
  to a long-lived daemon is the only way to let humans, agents, and
  scripts cohabit. The cost is one extra process to manage; the payoff
  is everything else in this document.

- **Plain JSON-RPC 2.0 over NDJSON, not gRPC or MCP.** Debuggability
  beats elegance for a tool that field-engineers will be poking at with
  `socat` and `nc` at 2 AM. Every message is a line of human-readable
  JSON. No codegen, no schema files, no toolchain. The performance
  penalty is invisible at serial-line speeds.

- **UDS by default, TCP opt-in.** PostgreSQL/MySQL/Redis convention.
  Serial ports are a developer's local resource; binding them to the
  network unintentionally is a foot-gun. `--tcp` is one short, explicit
  flag.

- **Ring-buffer fan-out, not stream-per-consumer.** A serial port is
  inherently a single source of truth. Modeling it as a single
  authoritative buffer with multiple consumer cursors keeps the protocol
  small and ordering trivial.

- **`run` over `send` + `expect`.** Two-step automation is a race in
  disguise. The client writes, then asks "did you match yet?" — but the
  daemon may already have matched something from before the write,
  matched only part of the response, or have been preempted by another
  client's send. `run` punts the whole thing to the daemon, which knows
  the precise byte position before its own write.

## What it isn't

- **Not a terminal emulator.** The interactive shell is good enough for
  90 % of board-bring-up work, but it isn't `xterm`. ANSI handling is
  passthrough; complex full-screen TUIs on the device may not render
  perfectly.

- **Not a multiplexer of multiple devices.** One daemon = one serial
  port. Run two daemons on different sockets/ports if you have two
  boards.

- **Not a recorder/replayer.** `tether tail | tee` gets you a flat log.
  Time-stamped or replayable session capture is left to whatever logging
  tooling you already use.

- **Not for high-rate binary protocols.** It works fine for any baud
  most embedded boards use, but if you're streaming 4 Mbps of binary
  packets you'll want a purpose-built tool, not a JSON-wrapped pipe.

## Where to go next

- Need to actually invoke the tool? → [AGENT_USAGE.md](AGENT_USAGE.md)
  is the cookbook with exit codes, JSON schema, and copy-paste recipes.
- Writing a non-Rust client or curious about the wire format? →
  [PROTOCOL.md](PROTOCOL.md) is the full v1 spec.
- Want a working example? → [`examples/`](../examples/) has five
  end-to-end Bash scripts (U-Boot bdinfo, set-and-verify env vars,
  parse the help command, wait for boot, detect shell type).

## Glossary

- **Daemon** (`tetherd`) — long-lived process that owns the serial port.
- **Client** (`tether`) — short-lived process that attaches to the daemon.
- **Session** — an attached client's record at the daemon: id, cursors,
  mode, flow-control.
- **Ring buffer** — the daemon's circular byte buffer of recent device
  output. Default 64 KiB. Drops oldest bytes when full; sessions that
  fall behind get a `lag` notification.
- **Consumer cursor** — per-session pointer used by `run`/`expect` for
  matching response data.
- **Notify cursor** — per-session pointer used by `data` push
  notifications.
- **Writer lock** — exclusive lock acquired by `run`, ensuring one
  client's send-and-match pair is atomic.
- **Transport** — UDS, TCP, or future Named Pipe; all carry the same
  NDJSON wire format.
- **Auth token** — random string required by TCP clients in their first
  `hello` message.
