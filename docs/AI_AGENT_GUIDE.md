# Teaching your AI agent to drive Serial Tether

> **Audience:** humans who want Claude Code / Codex / Cursor / etc. to drive
> an embedded board's serial console via Serial Tether.
>
> **Already-an-agent? You don't need this file.** Run `tether agents` (no
> daemon needed) or read [AGENT_USAGE.md](AGENT_USAGE.md) instead — that's
> the cookbook the agent itself should consume.

This doc covers four things:

1. The 30-second on-ramp — one line, no paste-and-go required.
2. How to pin it permanently into your project (`AGENTS.md` / `CLAUDE.md`),
   for when you want more than the on-ramp gives you for free.
3. A verification script you (or the agent) can run to prove it works.
4. Failure modes you should know about, so you can unstick the agent fast.

---

## 1. The 30-second on-ramp

If the daemon is already running on the host the agent can reach:

```
tetherd -D /dev/ttyUSB0 -b 115200          # local
# or
tetherd -D /dev/ttyUSB0 --tcp --auth-token MYSECRET   # remote-reachable
```

…paste this verbatim into your agent's chat window:

> You have access to `tether`, the CLI for Serial Tether — a daemon that owns
> a serial port and lets multiple clients share it. The daemon is already
> running. Run `export TETHER_NONINTERACTIVE=1`, then `tether agents` — it
> prints the full cookbook (built into the binary, no network needed, always
> matches the CLI you're running). Read it before your first real command.
>
> First steps: `tether --json status` to confirm the daemon and device, then
> `tether -d <id> exec "<cmd>"` for anything at a shell prompt (busybox,
> dash, bash, U-Boot hush). For a raw console with no shell — bootloader
> mid-boot, vendor monitor — use
> `tether -d <id> run "<cmd>" -u "<prompt-regex>" --literal --newline cr`
> instead.

That's enough for a competent coding agent to take it from there — `tether
agents` is self-contained and covers `exec` vs `run`, exit codes, the `-d`
discipline, and common pitfalls in one place.

For remote daemons add:

> The daemon is reachable at `tcp://<host>:5557` with token `MYSECRET`. Pass
> `-s tcp://<host>:5557` and either `--auth-token MYSECRET` or set
> `TETHER_AUTH_TOKEN=MYSECRET` in the environment.

---

## 2. Pin it permanently in `AGENTS.md` / `CLAUDE.md`

The on-ramp above (tell the agent to run `tether agents`) is enough for most
projects — it re-reads the current cookbook fresh every session, so there's
nothing to keep in sync. Pin a block into your project instead when you want
something the live command can't give you: a fixed device id baked in, a
non-default socket/TCP endpoint, or non-interactive mode without relying on
the agent remembering to export it.

Add this to whichever file your runner auto-loads (`CLAUDE.md`, `AGENTS.md`,
`GEMINI.md`, etc.) — it's intentionally short, because `tether agents`
carries the rest:

```markdown
## Serial console

Board `<id>` is on Serial Tether. The daemon (`tetherd`) is already running;
you connect with the `tether` CLI.

    export TETHER_NONINTERACTIVE=1     # once per session — never prompt
    tether -d <id> exec "<cmd>"        # shell console: output + real exit code
    tether -d <id> run "<cmd>" -u "<prompt-regex>" --literal --newline cr --json
                                        # raw console (bootloader, U-Boot w/o hush)

- Always pass `-d <id>` explicitly — never rely on the daemon's default device.
- Prefer `exec` for anything sitting at a shell prompt. Fall back to `run`
  only for consoles with no shell to run `echo`/`$?`.
- `--json` whenever you need to parse the result; read `result.output`
  (ANSI/echo already stripped), not `result.before`.
- Exit codes: `0` ok, `4` device disconnected, `6` lock contention (another
  session is writing), `7` unauthorized (TCP), `8` `exec` ran but the shell
  gave no numeric status (non-POSIX console), `124` timeout.
- Full cookbook: run `tether agents`, or read
  https://github.com/hulryung/serial-tether/blob/main/docs/AGENT_USAGE.md
```

If your project sometimes targets a non-default daemon (different socket
path, TCP, multiple boards), add a line alongside:

```markdown
- Daemon socket: `tcp://lab-host.local:5557`, token in `${TETHER_AUTH_TOKEN}`.
- Or, multiple local boards: pass `--name board0` / `--name board1` to every
  `tether` call. (Operator started each daemon with the matching `--name`.)
```

---

## 3. Verifying the agent can drive the board

Ask the agent to run this loop. It's exec-first — no prompt detection needed
for the common case:

```
1. tether --json status
   → expect: "device": {"connected": true, "path": "<the path you expected>", ...}

2. tether -d <id> exec "echo READY && uname -a"
   → expect: prints READY + kernel/version info, exits 0.
     This works whether the shell is bash/dash/busybox or hush-enabled
     U-Boot — no prompt regex to write.
```

If the device has no shell at all (bootloader mid-boot, vendor monitor, a
U-Boot built without hush), step 2 times out instead — fall back to:

```
3. tether -d <id> sync --idle-ms 500 --timeout-ms 3000
   → expect: a non-empty prompt candidate string. Save it.

4. tether -d <id> --json run "version" -u "<PROMPT>" --literal --newline cr --timeout-ms 3000
   → expect: matched=true, output containing the board's version banner.
```

If step 1 fails: the daemon isn't running on a path the agent can reach.
If step 2 times out: not a POSIX-ish shell — either it's a raw console (use
steps 3-4), or the device needs `shell=uboot`/`shell=none` registered (see
`tetherd --help`) so `exec` knows what it's talking to instead of guessing.

---

## 4. Failure modes the agent will hit

Tell the agent to recognize these and self-correct rather than retrying
blindly:

- **Hangs instead of erroring** — a multi-device daemon with no `-d`, under
  a harness-allocated PTY, showing an interactive device picker that will
  never get a keypress. Fix: `TETHER_NONINTERACTIVE=1` must be set (or
  `--no-interactive` passed) — always, not just when it seems to be needed.
- **`exit 124` (timeout)** — for `exec`, usually means the device isn't at a
  shell prompt (see §3 fallback). For `run`, first suspect `--newline`
  (embedded consoles usually want `cr`), then a wrong `-u` pattern, then
  raise `--timeout-ms` if the command is genuinely slow.
- **`exit 4` (device disconnected)** — USB unplug, board reset, or driver
  hiccup. The agent can call `tether -d <id> reconnect --timeout-ms 5000`
  and retry, or add `--auto-reconnect` to ride out a transient one.
- **`exit 6` (lock contention)** — another session holds the writer lock
  (likely mid-flash). `tether` prints `device is locked by another session
  (flashing?) — try again after it unlocks` to stderr. Pass `--preempt
  queue` (default) to wait, or `--preempt force` only if the agent is the
  authoritative caller and knows it's safe to abort the other session.
- **`exit 8` / `exit_code: null`** (`exec` only) — the command ran and its
  output was captured, but the device shell didn't report a numeric `$?` (a
  non-POSIX console, classically U-Boot without hush). Not a failure of the
  command itself — see [EXEC_NONPOSIX_SHELLS.md](EXEC_NONPOSIX_SHELLS.md).
  Register `shell=uboot` or fall back to `run`.
- **U-Boot runs every command twice** — `--newline crlf` was sent to it (CR
  runs the line, the trailing LF repeats it). Use `cr`, or register the
  device once with `shell=uboot`, which enforces CR-only framing for
  `exec`/`run` automatically from then on.
- **Garbled bytes / random characters** — wrong baud. The agent should call
  `tether -d <id> status` to read the configured baud live, then
  `tether -d <id> config --baud <N>` if it has authorization to change it.
- **stderr says "attaching as a client — no new daemon spawned"** — not an
  error, but a signal: a daemon was already running for this device, so the
  agent's `tether -D <PATH>` got auto-redirected to it. From this point on
  the agent should drop `-D` and just use `tether -d <id> <subcommand>` (the
  redirect already populated the right id). Don't try to "kill" the existing
  daemon; the user's interactive session may be on it.

For a longer treatment, the agent should read [AGENT_USAGE.md §Common
pitfalls](AGENT_USAGE.md#common-pitfalls) and [§Connecting when a daemon
may already be running](AGENT_USAGE.md#connecting-when-a-daemon-may-already-be-running).

---

## 5. Tips per agent runner

These are not requirements — Serial Tether doesn't care which model is
calling it — but they're worth knowing. In every case, set
`TETHER_NONINTERACTIVE=1` in the runner's environment (not just in the
pasted prompt) so a stray interactive picker can never block the session.

**Claude Code / Codex (terminal agents):** they shell out to `tether`
directly. Export `TETHER_NONINTERACTIVE=1` in the shell profile or session
env the agent inherits. The `--json` mode produces stable schema output that
maps cleanly to their tool-reasoning. They'll run `tether agents` on their
own once told to, and remember it for the session.

**Cursor / IDE agents with shell access:** same as above, but you may need
to ensure the `tether` binary is in `PATH` for the IDE's spawned shell
(`brew install hulryung/tap/serial-tether` or
`cargo install serial-tether` puts it in a standard location), and that
`TETHER_NONINTERACTIVE=1` is in whatever env the IDE passes to its shell —
IDE-spawned shells don't always inherit your interactive shell's exports.

**Agents without shell access:** use the wire protocol directly over a
TCP socket — see [PROTOCOL.md](PROTOCOL.md). Every CLI command corresponds
to a JSON-RPC method (`run`, `send`, `status`, …); `exec` is the one
exception built entirely client-side (a wrapper around `run`), so a raw
protocol client should reimplement it as send + expect on begin/end markers,
or just use `run` directly with its own prompt detection.

---

## TL;DR

1. Run `tetherd -D /dev/ttyUSB0`.
2. Tell the agent: `export TETHER_NONINTERACTIVE=1 && tether agents` — read
   that, then go. (Pin the §2 block into `AGENTS.md`/`CLAUDE.md` only if you
   need more than that.)
3. Ask the agent to run the two-step verification from §3 (`status` +
   `exec`).
4. Hand it the actual task.

That's the whole setup.
