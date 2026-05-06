# Teaching your AI agent to drive Serial Tether

> **Audience:** humans who want Claude Code / Codex / Cursor / etc. to drive
> an embedded board's serial console via Serial Tether.
>
> **Already-an-agent? You don't need this file.** Read
> [AGENT_USAGE.md](AGENT_USAGE.md) instead — it's the cookbook the agent
> itself should consume.

This doc covers four things:

1. The 30-second on-ramp — copy/paste a prompt and you're done.
2. How to wire it permanently into your project (`AGENTS.md` / `CLAUDE.md`).
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
> running. Read the cookbook at
> https://github.com/hulryung/serial-tether/blob/main/docs/AGENT_USAGE.md
> before you call it for the first time. Use `tether --json` for everything
> and prefer one transactional `tether run "<cmd>" -u "<prompt>" --literal
> --newline crlf --timeout-ms 5000` per command — never `send` + `expect`
> separately, never interactive shell mode.
>
> First steps: `tether --json status` to confirm the daemon, then `tether
> sync --idle-ms 500` to detect the prompt, then your first `run`.

That's enough for a competent coding agent to take it from there.

For remote daemons add:

> The daemon is reachable at `tcp://<host>:5557` with token `MYSECRET`. Pass
> `-s tcp://<host>:5557` and either `--auth-token MYSECRET` or set
> `TETHER_AUTH_TOKEN=MYSECRET` in the environment.

---

## 2. Make it permanent in `AGENTS.md` / `CLAUDE.md`

Most agent runners (Claude Code, Codex, Cursor) auto-load a markdown file
from your project root that primes the agent on every session. Add this
block to whichever file your runner uses (`CLAUDE.md`, `AGENTS.md`,
`GEMINI.md`, etc.):

```markdown
## Serial console

We talk to the board over `/dev/ttyUSB0` via **Serial Tether**. The daemon
(`tetherd`) is already running; you connect with the `tether` CLI.

**Canonical command** — use this for 95% of cases:

    tether --json run "<COMMAND>" --newline crlf -u "<PROMPT>" --literal --timeout-ms 5000

- `--json` always; never parse human-readable output.
- `--literal` because prompts (`uboot >`, `# `, etc.) contain regex
  metacharacters by default.
- `--newline crlf` for U-Boot / Linux serial; `--newline lf` if the device
  uses Unix line endings.
- Always pass `--timeout-ms` explicitly.

**Read the response from `result.output`** — it's UTF-8 with ANSI escapes
and the echoed command stripped. Use `result.match` to confirm which prompt
fired.

**Discover before you assume.** First call should be:

    tether --json status               # confirm daemon + device + baud
    tether sync --idle-ms 500          # auto-detect the current prompt

**Don't:**

- Don't use `tether shell` (interactive only — for humans).
- Don't compose `send` + `expect` yourself; `run` is atomic.
- Don't drop `--timeout-ms`; default is 3 s and will silently truncate
  long-running commands.

**Cookbook & exit-code reference:**
https://github.com/hulryung/serial-tether/blob/main/docs/AGENT_USAGE.md

**Wire protocol (JSON-RPC over NDJSON), if you need to bypass the CLI:**
https://github.com/hulryung/serial-tether/blob/main/docs/PROTOCOL.md
```

If your project sometimes targets a non-default daemon (different socket
path, TCP, multiple boards), add a one-line note alongside:

```markdown
- Daemon socket: `tcp://lab-host.local:5557`, token in `${TETHER_AUTH_TOKEN}`.
```

That single section is enough — the agent will follow the link to
`AGENT_USAGE.md` on its first call and you're done.

---

## 3. Verifying the agent can drive the board

Ask the agent to run this loop verbatim. If it gets all four green, it's
talking to the board correctly:

```
1. tether --json status
   → expect: "device.connected": true, the path you expected.

2. tether --json sync --idle-ms 500 --timeout-ms 3000
   → expect: a non-empty "prompt_candidate" string. Save it; this is the
     prompt for subsequent runs.

3. tether --json run "version" --newline crlf -u "<PROMPT>" --literal --timeout-ms 3000
   → expect: matched=true, duration_ms < 1000, output containing the
     board's version banner.

4. tether --json run "<some quick command>" --newline crlf -u "<PROMPT>" --literal --timeout-ms 5000
   → expect: matched=true, output containing the expected response.
```

If step 1 fails: the daemon isn't running on a path the agent can reach.
If step 2 fails: the device is wedged or the baud is wrong.
If step 3 fails: usually `--newline` mismatch (try `lf` instead of `crlf`).

---

## 4. Failure modes the agent will hit

Tell the agent to recognize these and self-correct rather than retrying
blindly:

- **`exit 124` (timeout)** — first thing to suspect is `--newline`. After
  that, raise `--timeout-ms` if the command is genuinely slow.
- **`exit 4` (device disconnected)** — USB unplug, board reset, or driver
  hiccup. The agent can call `tether reconnect --timeout-ms 5000` and retry.
- **`exit 6` (lock contention)** — another client is mid-transaction. Pass
  `--preempt queue` (default) to wait for it, or `--preempt force` if the
  agent is the authoritative caller.
- **`output` is empty but `matched: true`** — the prompt fired immediately
  with nothing in front of it. Either the previous run left no output, or
  the device echoed and the echo was stripped.
- **Garbled bytes / random characters** — wrong baud. The agent should
  call `tether config` to read live, then `tether config --baud <N>` if it
  has authorization to change.

For a longer treatment, the agent should read [AGENT_USAGE.md §Common
pitfalls](AGENT_USAGE.md#common-pitfalls).

---

## 5. Tips per agent runner

These are not requirements — Serial Tether doesn't care which model is
calling it — but they're worth knowing.

**Claude Code / Codex (terminal agents):** they shell out to `tether`
directly. The `--json` mode produces stable schema output that maps
cleanly to their tool-reasoning. They will pick up the `AGENTS.md` /
`CLAUDE.md` block on the first turn and remember it for the session.

**Cursor / IDE agents with shell access:** same as above, but you may need
to ensure the `tether` binary is in `PATH` for the IDE's spawned shell
(`brew install hulryung/tap/serial-tether` or
`cargo install serial-tether` puts it in a standard location).

**Agents without shell access:** use the wire protocol directly over a
TCP socket — see [PROTOCOL.md](PROTOCOL.md). Every CLI command corresponds
to a JSON-RPC method (`run`, `expect`, `send`, `status`, …).

---

## TL;DR

1. Run `tetherd -D /dev/ttyUSB0`.
2. Paste the AGENTS.md / CLAUDE.md block from §2 into your project.
3. Ask the agent to run the four-step verification from §3.
4. Hand it the actual task.

That's the whole setup.
