# Changelog

All notable changes to **serial-tether** are tracked here. The format is loosely
based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the
project follows [SemVer](https://semver.org/spec/v2.0.0.html). Once 1.0 is
cut, the protocol stability commitments documented in `docs/PROTOCOL.md` §10
also apply.

## [Unreleased]

(Nothing yet.)

## [0.11.0] — 2026-07-05

### Added
- **Per-device console personality (`shell=` / `prompt=` / `newline=`).**
  Register what's on the other end of the wire once, in the `-D` spec:
  ```sh
  tetherd -D 'board=/dev/ttyUSB0,shell=uboot,prompt==> '
  # shell=posix|uboot|none (default posix); prompt=<regex>; newline=lf|cr|crlf|none
  ```
  `shell=uboot` forces CR-only framing for `exec`/`run` (CRLF double-executes
  U-Boot's CLI) and defaults `newline=cr`; `shell=none` makes `exec` refuse
  immediately with the raw-console recipe instead of timing out; `prompt=`
  becomes the default `-u` for `run`/`sync`. The fields are reported in
  `list_devices` / `status` / `hello`.
- **`exec` now runs on U-Boot (hush) and reports honest exit codes.** The
  wrapper is a single line with no temp variable, so it works unchanged on
  POSIX shells and hush-enabled U-Boot. When the console can't report a numeric
  status (non-POSIX monitor), `exec` still returns the captured output but
  reports the status as *unknown*: `exit_code: null` in `--json` and process
  exit **8** (new exit code) — it never fabricates a `0`. Details in
  `docs/EXEC_NONPOSIX_SHELLS.md`.
- **`tether pty` — on-demand virtual serial ports, one per tool.** Each
  invocation creates a client-side PTY bridged to the device over the normal
  protocol, with its own session cursor — so every tool gets a **full copy** of
  the stream (two tools must never share one port: simultaneous readers split
  the bytes between them, measured 128/72 of 200). Because the PTY is created
  client-side, it works over TCP: a board on a lab host becomes a local port on
  your laptop.
  ```sh
  tether -d a35 pty -- minicom -D {}     # port lives as long as minicom; {}=path, $TETHER_PTY set,
                                         # child's exit code passes through
  tether -d a35 pty --link /tmp/a35.pty  # or: print path on stdout (after ready, flushed), Ctrl-C to stop
  tether -d a35 pty --read-only          # observation-only (typed bytes dropped, counted on stderr)
  tether -d a35 pty --lock -- flasher {} # exclusive writer (see lock/unlock below)
  ```
  Safety details: links are claimed atomically (`symlink(2)`, EEXIST → next
  slot) with a `<link>.pid` sidecar so stale links from killed clients are
  detected and reclaimed instead of pointing at a recycled pty; SIGINT/SIGTERM
  and all error paths remove the link; the bridge re-asserts raw mode if a tool
  flips the slave into cooked/echo mode (prevents echo storms and CR/LF
  munging); master writes are timeout-bounded so a stopped tool can't wedge the
  bridge (dropped bytes are counted and reported). Startup order is a contract
  for CI: pty → link → attach → (lock) → path printed+flushed → bridge.
  Integration tests `client_pty_bridges_and_cleans_up_on_sigterm`,
  `client_pty_lock_blocks_other_writers`; unit tests for link allocation,
  `{}` substitution, and sidecar staleness.
- **`lock` / `unlock` RPCs — real exclusive write for flashing.** `tether pty
  --lock` (or the RPCs directly) takes a session-held *exclusive* writer lock:
  while held, `send` from any other session fails with `-32004 lock_contention`
  and the daemon-side `pty=` bridge drops non-tether tool bytes instead of
  interleaving them into the flash stream. `run`'s internal transactional hold
  is unchanged (non-exclusive) so existing behavior doesn't regress. The lock
  releases on `unlock`, session detach, or connection teardown — a crashed
  flashing client can't strand the device locked. Documented in PROTOCOL.md
  §6.14; covered by `tests/lock.rs` and 15 new lock-state unit tests.

- **Virtual serial port (PTY) so non-tether tools can share a device.** Add
  `pty` (or `pty=/path`) to a `-D` spec and `tetherd` publishes a virtual serial
  port — a stable symlink (`/tmp/tether-<id>.pty` by default) that minicom,
  screen, pyserial, or a UART flasher can open like a real port. The daemon
  bridges raw bytes both directions, and tether clients keep tailing/sending
  concurrently — no second process fighting for `/dev/cu.*`.
  ```sh
  tetherd -D 'a35=/dev/cu.usbserial-XXXX,pty' -b 115200
  # → virtual port: a35 -> /tmp/tether-a35.pty
  screen /tmp/tether-a35.pty 115200
  ```
  **Carries data only.** A PTY has no modem lines or real line rate, so DTR/RTS
  auto-reset and mid-stream baud changes can't pass through it (verified: macOS
  PTYs return `ENOTTY` for all modem-control ioctls on both ends — an OS limit,
  not fixable in software). Console sharing, and flashing where the board enters
  download mode out-of-band (boot strap / button / command), work today. For
  reset-based flashing use `tether reset` (below). Integration test
  `pty_bridge_shares_port_both_directions`.
- **`tether reset` — board reset for flashing through a shared port.** Since a
  PTY can't carry DTR/RTS, drive the reset on the *real* port instead, then flash
  with the tool's no-auto-reset option pointed at the virtual port:
  ```sh
  tether -d esp reset --esp32           # or --seq "dtr=0 rts=1 wait=100 dtr=1 rts=0 wait=50 dtr=0"
  esptool --port /tmp/tether-esp.pty --before no_reset write_flash ...
  ```
  `--esp32` is the esptool "classic" sequence; `--seq` takes explicit steps
  (`dtr=0|1`, `rts=0|1`, `wait=<ms>`) for other wirings. Built on the existing
  `set_dtr`/`set_rts` RPCs — no daemon change, and it works while a `pty` bridge
  is active (line control and data are independent). Unit-tested sequence parser.

### Fixed
- **`tetherd` no longer exits silently when `accept()` fails.** A single
  transient accept error (EMFILE under fd pressure, ECONNABORTED from a peer
  resetting mid-handshake) used to end the listener task, which ended `main()`
  — the whole daemon, including every live session, went down with exit 0, no
  message, and a stale socket file left behind. Accept errors are now logged
  and retried (with a short backoff). Regression test
  `survives_accept_error_under_fd_exhaustion` lowers `RLIMIT_NOFILE` and
  verifies the daemon keeps serving through 200 concurrent connections.
- **`tether status` now honors `-d <id>`.** It used to always report the
  daemon's default device regardless of `-d` (silently, even for nonexistent
  ids) — which broke the documented `-d <id> --json status | jq .device.path`
  recipe on multi-device daemons. With `-d`, the top-level
  `device`/`buffer`/`lock`/`sessions` fields now describe the requested
  device; unknown ids fail with the list of ids the daemon does have.
- **Exit codes 5 and 6 are no longer silent.** Buffer overflow and lock
  contention each print a one-line stderr explanation instead of exiting with
  nothing on any stream.

### Docs
- **New user documentation suite**, organized easy → advanced:
  `docs/GETTING_STARTED.md` (install → first session in ~10 minutes),
  `docs/COOKBOOK.md` (16 task recipes from boot-log capture to flashing
  through a shared port), `docs/CLI_REFERENCE.md` (every flag of both
  binaries, generated against `--help`), `docs/TROUBLESHOOTING.md`
  (symptom → cause → fix, exact error strings), and `docs/README.md`
  (learning-path index). Command sequences were verified against the built
  binaries (socat-backed fake devices, pyserial round trips).
- **Documentation website** at https://hulryung.github.io/serial-tether/ —
  VitePress rendering `docs/` in place (same files stay readable on GitHub),
  with local full-text search, dark/light brand theme, and the asciinema
  demos ported from the old hand-made landing page onto the new home page.
  Deployed by `.github/workflows/docs.yml` on every push to `main` that
  touches `docs/`.
- `docs/AI_AGENT_GUIDE.md` refreshed to the current CLI (exec-first,
  `tether agents` on-ramp, `TETHER_NONINTERACTIVE=1`); removed a ghost
  `--log-protocol` flag from PROTOCOL.md/OVERVIEW.md that was never shipped.

- **Daemon `pty=` bridge no longer wedges when no tool is reading.** The
  device→pty write could park forever once the tiny kernel pty buffer (~1KB on
  macOS) filled, freezing the ring cursor and replaying stale bytes to the next
  tool. Writes are now timeout-bounded (200ms): on timeout the chunk is dropped
  (never retried — a cancelled write may have delivered a prefix, and retrying
  would duplicate it), counted, and reported via a rate-limited warning.

## [0.10.0] — 2026-06-24

### Added
- **`tether agents` — a built-in, ready-to-use cookbook for AI agents.** Tell an
  agent to run `tether agents` and it prints (to stdout, no daemon needed) the
  canonical commands (`-d <id> exec …`), the non-interactive contract, the exit
  codes, remote-over-TCP usage, and the common pitfalls — current with the CLI
  and fine to paste into an `AGENTS.md` / `CLAUDE.md` block.
- **Non-interactive kill switch for agents/automation.** `--no-interactive` (or
  `TETHER_NONINTERACTIVE=1`) disables the port/device pickers and falls back to
  the usual error, so a harness that runs under a PTY can't get stuck on a
  prompt. (Pickers were already TTY-gated; this is the explicit override for
  PTY-allocating agents. The env var accepts `1`/`true`/`yes`, etc.)
- **Zero-config `tether`: auto-start and interactive device selection.** You no
  longer need to know whether a daemon is running. Running a device-targeting
  command with no endpoint and no `-d`:
  - **No daemon?** `tether` discovers the USB serial ports and, in an
    interactive terminal, lets you pick one (auto-selected if there's exactly
    one), then opens it as a session-scoped daemon — like `tio /dev/ttyUSB0`,
    gone when you exit. macOS dial-in (`/dev/tty.*`) duplicates and virtual
    ports are filtered out so the menu only lists real adapters (`/dev/cu.*`,
    `/dev/ttyUSB*`).
  - **Daemon with several devices and no `-d`?** It prints a numbered menu of
    the managed devices and uses your pick (Enter selects the first).
  Both prompts only appear on an interactive TTY; pipes and scripts keep the
  previous behavior exactly — the "no daemon" hint or the `AmbiguousDevice`
  error — so automation is never blocked on a prompt. Long-lived shared daemons
  started explicitly with `tetherd` are unchanged.

### Fixed
- **`config` now respects `-d <id>` on multi-device daemons.** The read-only
  form was reading the daemon's *default* device from `status`, and the
  set form (`config --baud …`) was applying to the default device, both
  ignoring `-d`. They now target (and display) the selected device.

## [0.9.5] — 2026-06-04

### Changed
- **Tidier per-command `--help`.** Every option now carries a one-line
  description, so `tether <cmd> -h` no longer shows bare `[default: …]` entries
  with no explanation. In particular `tail --from` documents its `now` / `start`
  values (and now rejects anything else up front), and the undocumented options
  on `expect`, `run`, `exec`, and `sync` — timeouts, `--literal`, `--strip-ansi`,
  `--max-output-bytes` — gained short help. Positional arguments (`send`,
  `expect`, `run` data/pattern) are described too. `tail`'s summary now spells
  out that it's read-only, follows like `tail -f`, and splits data→stdout /
  events→stderr.

## [0.9.4] — 2026-06-04

### Added
- **`tether exec "<cmd>"` — run a shell command, capture just its output.** For
  the common automation need ("send a command, get only its output back"),
  `exec` wraps the command so the device shell brackets the output with two
  unique markers and reports the command's exit status, then returns only the
  bytes between them. No prompt pattern to guess and no BEGIN/END scaffolding to
  hand-roll. The echoed command line is dropped even when the device terminal
  wraps it across columns (begin/end markers are matched in the shell's
  *evaluated* output, not its echo, via an empty-quote `BE""G` split). Output
  goes to stdout and `tether exec` exits with the *device command's* status,
  ssh-style; `--json` yields `{output, exit_code, duration_ms}`. Assumes a
  POSIX-ish shell (busybox, dash, bash, U-Boot hush) at a prompt — for raw
  consoles use `send` + `expect` / `run`. New integration tests
  `exec_captures_output_and_zero_exit` and `exec_mirrors_nonzero_exit_and_json_shape`.

### Fixed
- **Out-of-order / garbled writes when a connection pipelines requests.** The
  daemon used to dispatch *every* request on its own task, so a burst of
  `send`s on a single connection (a pasted multi-line block, or an agent
  pipelining commands) raced at the device writer and could reach the wire out
  of order — observed as a rotated or garbled command line, and worse with
  longer input since more chunks raced. Requests are now handled inline, in
  arrival order; only the methods that genuinely block on device output
  (`expect`, `run`, `reconnect`) still run on their own task, so a long
  `expect` doesn't stall the rest of the connection. New integration test
  `pipelined_sends_reach_the_wire_in_order` fires 64 sends back-to-back on one
  connection and asserts the bytes arrive in issue order.

### Changed
- **`reconnect` now surfaces *why* the port is still down.** When a reconnect
  times out with the device still disconnected, the daemon includes the last
  disconnect reason (e.g. `No such file or directory`) in its reply and the
  client prints it, so a vanished device path is diagnosed immediately instead
  of a bare `still disconnected after Nms`.
- **`-D <device-id>` mistakes get a pointed hint.** When `-D <VALUE>` fails to
  start an embedded daemon and `VALUE` isn't a path (no `/`, nothing by that
  name on disk), the error now suggests lowercase `-d <VALUE>` — the flag for
  selecting a device on an already-running daemon — instead of leaving the
  user puzzling over why a device-id couldn't be opened as a file.
- Internal: rewrote the daemon-probe `for … { return }` loop as an iterator
  `.next()` so `cargo clippy` (which denies `clippy::never_loop`) is clean again.

## [0.9.3] — 2026-05-20

### Added
- **`tether <PATH> --tcp` — one-liner shell + TCP exposure.** While the
  client is in standalone mode (a path positional or `-D <PATH>`), it
  now also forwards `--tcp [<HOST:PORT>]` and `--auth-token <TOKEN>` to
  the embedded daemon. Use `--tcp=HOST:PORT` for an explicit bind
  (equals is required so a bare `--tcp` followed by a subcommand like
  `status` isn't mis-parsed as the value). Before the shell starts,
  stderr prints the listener address, the token (auto-generated if
  omitted), and the `tether -s tcp://… --auth-token …` snippet a
  remote agent should use. The TCP listener follows the embedded
  daemon's ephemeral lifespan — quit the shell, the daemon stops, and
  remote clients see their connection drop.
- New integration test
  `standalone_with_tcp_exposes_remote_attachable_listener`: spawns
  `tether <PTY> --tcp=… --auth-token …`, then verifies a separate
  `tether -s tcp://…` client can attach to the same embedded daemon.

### Changed
- **`tetherd --help` is grouped + has a doc-link footer**, matching the
  `tether --help` polish from v0.9.1. Options are bucketed into
  `Device(s)` / `Serial defaults` / `Listeners` / `TCP auth` / `Buffer`;
  the footer prints `EXAMPLES`, the full `-D` spec grammar, and
  `LEARN MORE` links to `AGENT_USAGE.md` / `PROTOCOL.md` / repo URL.
- **Fan-out drain loop no longer sleeps with un-emitted data buffered.**
  `conn.rs` now drains every session before re-entering `buffer.wait()`,
  closing a small race window where `RingBuffer::push`'s
  `notify_waiters()` could be lost while a fan-out task was mid-encode.
  Wire format unchanged; no user-facing behavior change for normal
  workloads (the burst-y "plays then pauses" cadence operators have
  reported turned out to be FTDI USB IN polling + board-side
  kernel-printk spooling, not the daemon).
- README "Remote daemon (TCP)" gains a "Quick share from a standalone
  session" subsection documenting the new one-liner; the prior
  "intentional split" note has been retracted accordingly.

## [0.9.2] — 2026-05-09

### Changed
- **`tether -D <PATH>` (and the bare-path shorthand) auto-redirects to
  an existing daemon.** Before this release, invoking standalone mode
  while a long-lived `tetherd` was already managing the device would
  spawn a second daemon — both processes then fought for the port,
  garbling whatever interactive shell the operator had open. The CLI
  now probes `/tmp/tetherd*.sock` first, finds the daemon already
  managing the requested OS path, and attaches as a client to that
  daemon (printing a one-line `attaching as a client — no new daemon
  spawned` notice on stderr). Multi-device daemons additionally get
  the matching `device_id` filled in automatically, so subsequent
  commands route correctly.

### Added
- Integration test `standalone_redirects_to_existing_daemon` exercises
  the redirect end-to-end against a real socat PTY pair.
- AGENT_USAGE.md gains a "Connecting when a daemon may already be
  running" section; AI_AGENT_GUIDE.md picks up matching guidance under
  Don'ts / failure modes so the AI can interpret the redirect notice.

## [0.9.1] — 2026-05-07

### Changed
- **`tether --help` is much easier to scan.** Subcommands reordered into
  Interactive / Scripted RPCs / Inspection / Line control / Lifecycle
  groups (same set, just easier to find). Global options gain
  `help_heading` so clap renders them under Endpoint / Device target /
  Output / TCP auth / Standalone mode sections instead of one flat list.
  First sentences of `shell`, `reconnect`, `disconnect`, `connect`, and
  `--auto-reconnect` trimmed so the short `-h` output stays compact.
- New `--help` footer with EXAMPLES, COMMANDS BY CATEGORY, and a
  LEARN MORE block of canonical doc links so AI agents calling
  `tether --help` discover `AI_AGENT_GUIDE.md`, `AGENT_USAGE.md`, and
  `PROTOCOL.md` without leaving the CLI.

No wire-format or behavior changes — purely the CLI surface getting
tidier on the path to 1.0.

## [0.9.0] — 2026-05-07

Stabilization release on the path to 1.0. No new user-facing features
or wire-format changes; this release is about putting safety nets under
everything that landed in 0.7 / 0.8 so the eventual 1.0 cut is just a
version-number flip.

### Added
- **Protocol stability commitment** documented in
  [`docs/PROTOCOL.md` §10](docs/PROTOCOL.md). Spells out exactly what
  freezes when 1.0 is tagged: `protocol_version` "1" stays put for the
  entire 1.x crate series, additive-only evolution rules per field
  class, and what counts as minor vs patch vs reserved-for-2.0.
- **End-to-end integration test suite**:
  `crates/serial-tether/tests/integration.rs` (7 tests, ~2s, runs as
  part of `cargo test`). Each test spawns its own `tetherd` against
  `socat`-created PTY pair(s) and asserts on JSON output / exit codes.
  Coverage: single-device flow, multi-device list-devices + per-device
  baud override, ambiguous-device error (-32015), tio control on PTY
  (-32007), disconnect/connect lifecycle, `tether <PATH>` shorthand,
  list_ports shape stability.
- **`CHANGELOG.md`** formalised (this file). README docs index links it.

### Changed
- `cargo clippy --workspace --all-targets` is silent. Cleared three
  accumulated warnings:
  - `emit_disconnect` flag in serial.rs was dead — dropped (the daemon
    already verifies the device opens once before spawning the owner
    task, so the suppression case it guarded can't happen).
  - `state.devices.values().cloned()` → `.values()` (Arc clone happens
    inside the loop body; outer `.cloned()` was redundant).
  - `std::io::Error::new(ErrorKind::Other, ...)` →
    `std::io::Error::other(...)`.

## [0.8.2] — 2026-05-07

### Added
- **tio-style quick-start.** `tether /dev/ttyUSB0` drops into an interactive
  shell with no daemon to set up — the bare path as the first argument
  auto-spawns a private `tetherd`. Detection is narrow (argv[1] must contain
  `/` or start with `~`); plain identifiers fall through to subcommand
  parsing. Existing `-D <PATH>` form is unchanged.
- `tether --help` long-about and the friendly daemon-not-running error
  now lead with the bare-path form.

## [0.8.1] — 2026-05-06

### Changed
- Re-cut release tied to the v0.8 demo refresh on the website (multi-device
  cast added, existing demos rebuilt with the v0.8.0 title bar, README/Pages
  restructured). No source changes vs 0.8.0.

## [0.8.0] — 2026-05-06

### Added
- **Multi-device daemon.** A single `tetherd` process can own N serial
  ports. Operator gives one `-D '[id=]path[,key=val,...]'` per device.
  Clients address each by id with `tether -d <id>`.
- New RPCs: `list_devices`, and per-device tio-style line control —
  `send_break`, `set_dtr`, `set_rts`, `read_modem_status`,
  `disconnect_device`, `connect_device`.
- New CLI subcommands: `list-devices`, `break`, `dtr`, `rts`, `lines`,
  `disconnect`, `connect`.
- New shell escapes inside `tether shell`: Ctrl-A B (break), D (toggle
  DTR), R (toggle RTS), L (modem status). Mirrors tio's Ctrl-T sequences.
- New error codes: `-32009 device_not_found`, `-32015 ambiguous_device`.
- Optional `device_id` field on every server→client notification so
  multi-device daemons can route them.
- `StatusResult` extended with `devices: Vec<DeviceStatus>` and
  `default_device`. Single `device` / `buffer` fields stay populated
  with the default device for v0.7-client compat.

### Changed
- `Device` struct extracted: per-device buffer / writer / serial-control
  mpsc / config / state / lock / event broadcast all live in `Device`.
  `DaemonState` is now `HashMap<id, Arc<Device>>` plus daemon-wide
  sessions and auth_token. `Session` carries `device_id`.

### Compatibility
- 100% backwards compatible: a multi-device daemon with exactly one
  device behaves identically to a v0.7 daemon for old clients.
  Protocol stays at version "1".

## [0.7.1] — 2026-05-06

### Added
- `--name <NAME>` on both `tetherd` and `tether`. The daemon defaults
  its UDS to `/tmp/tetherd-<NAME>.sock`; the client expands the same
  alias on the receiving end. Multiple per-board daemons can run side
  by side without colliding.

## [0.7.0] — 2026-05-05

### Added
- `list_ports` RPC — enumerate the serial ports the daemon's host
  knows about. CLI: `tether ports`.
- `set_device` RPC — apply a partial settings update (baud / data-bits /
  parity / stop-bits / flow_control) to the open port without dropping
  it. Real backend uses `tokio_serial` setters; PTY backend returns
  `-32007 unsupported_serial_op`. CLI: `tether config [--baud N ...]`.
- Shell escapes: Ctrl-A C (display config) / V (list ports).
- Banner now prints framing (`8N1 flow=none`).
- `tetherd` flags: `--data-bits`, `--parity`, `--stop-bits`,
  `--flow-control`.

## [0.6.0] — 2026-05-05

### Added
- Auto-reconnect on the daemon side: a single owner task wraps an outer
  reconnect loop around the per-session IO loop, with exponential
  backoff capped at 10s.
- `device` notifications (`disconnected`, `reconnected`) broadcast to
  every attached client.
- `reconnect` RPC + `--auto-reconnect` client flag for retry-on-disconnect.
- Standalone mode: `tether -D /dev/ttyUSB0` auto-spawns a private
  daemon, runs the requested command (or shell), and tears the daemon
  down on exit. Same UX as `tio`.

## Earlier (v0.x bring-up)

The v0.1–v0.5 series brought up the protocol skeleton, the writer-lock
state machine, ANSI/echo stripping, the ring-buffer fan-out, the
`tether shell` raw-mode client, and TCP transport with token auth.
See `git log --first-parent v0.6.0` for the full history.

[Unreleased]: https://github.com/hulryung/serial-tether/compare/v0.9.3...HEAD
[0.9.3]: https://github.com/hulryung/serial-tether/releases/tag/v0.9.3
[0.9.2]: https://github.com/hulryung/serial-tether/releases/tag/v0.9.2
[0.9.1]: https://github.com/hulryung/serial-tether/releases/tag/v0.9.1
[0.9.0]: https://github.com/hulryung/serial-tether/releases/tag/v0.9.0
[0.8.2]: https://github.com/hulryung/serial-tether/releases/tag/v0.8.2
[0.8.1]: https://github.com/hulryung/serial-tether/releases/tag/v0.8.1
[0.8.0]: https://github.com/hulryung/serial-tether/releases/tag/v0.8.0
[0.7.1]: https://github.com/hulryung/serial-tether/releases/tag/v0.7.1
[0.7.0]: https://github.com/hulryung/serial-tether/releases/tag/v0.7.0
[0.6.0]: https://github.com/hulryung/serial-tether/releases/tag/v0.6.0
