# Documentation

`tether` / `tetherd` let humans, scripts, AI agents, and other serial tools
share one serial port concurrently. The docs below are organized by what
you're trying to do, not by feature — pick your row and go.

## Learning path

| I want to... | Read |
|---|---|
| Get started from scratch | [GETTING_STARTED.md](GETTING_STARTED.md) — install, zero-config quick start, the four everyday commands |
| Do a specific task | [COOKBOOK.md](COOKBOOK.md) — task-oriented recipes, easy → advanced |
| Look up a command or flag | [CLI_REFERENCE.md](CLI_REFERENCE.md) — complete flag-by-flag reference for both binaries |
| Fix an error I'm seeing | [TROUBLESHOOTING.md](TROUBLESHOOTING.md) — symptom → cause → fix |
| Understand the architecture | [OVERVIEW.md](OVERVIEW.md) — the daemon/client model, why it's built this way |
| Set up an AI agent | [AI_AGENT_GUIDE.md](AI_AGENT_GUIDE.md) — wiring an agent up (AGENTS.md block, verification); the cookbook the agent itself reads is [AGENT_USAGE.md](AGENT_USAGE.md) / `tether agents` |
| Integrate programmatically / speak the wire protocol | [PROTOCOL.md](PROTOCOL.md) — JSON-RPC 2.0 / NDJSON spec |
| Run `exec` against U-Boot or another non-POSIX shell | [EXEC_NONPOSIX_SHELLS.md](EXEC_NONPOSIX_SHELLS.md) |

## What each doc covers

- **[GETTING_STARTED.md](GETTING_STARTED.md)** — first-time setup, from
  install through the four commands you'll use daily (`shell`, `tail`,
  `exec`, `config`), with copy-pasteable examples.
- **[COOKBOOK.md](COOKBOOK.md)** — recipes for specific jobs: flashing
  through a shared port, multi-board setups, CI, remote access.
- **[CLI_REFERENCE.md](CLI_REFERENCE.md)** — every flag and subcommand for
  `tether` and `tetherd`, generated against the actual `--help` output.
- **[TROUBLESHOOTING.md](TROUBLESHOOTING.md)** — error messages and exit
  codes you might hit, and what to do about each.
- **[OVERVIEW.md](OVERVIEW.md)** — the mental model: daemon owns the port,
  clients attach, why `run`/`exec` are race-free.
- **[AGENT_USAGE.md](AGENT_USAGE.md)** — one-page cookbook written for an
  AI agent to read directly (exit codes, JSON shapes, pitfalls).
- **[AI_AGENT_GUIDE.md](AI_AGENT_GUIDE.md)** — for the human doing the
  wiring: pointing an agent at a board, AGENTS.md/CLAUDE.md snippets,
  verifying it works, per-runner tips.
- **[PROTOCOL.md](PROTOCOL.md)** — the wire format, for anyone writing a
  non-Rust client.
- **[EXEC_NONPOSIX_SHELLS.md](EXEC_NONPOSIX_SHELLS.md)** — how `exec`
  behaves against U-Boot and other shells that can't report a numeric exit
  status.

New to the project? Start at [GETTING_STARTED.md](GETTING_STARTED.md) — the
rest of this table is for once you're past the first 10 minutes.
