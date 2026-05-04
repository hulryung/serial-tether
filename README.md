# Serial Tether

> A daemon and CLI that lets AI agents and humans share a single serial device.

`tetherd` owns the serial port; multiple clients (`tether`, user scripts, the future `tether-tui`) connect over a Unix socket / Named Pipe / TCP and read and write concurrently. The agent-facing CLI (`tether`) is transactional, structured, and bounded by design: JSON-RPC responses carry decoded text, exit codes follow shell conventions, the `run` primitive is race-free at the daemon level, and output truncation guards LLM context budgets.

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
cargo install --git https://github.com/hulryung/serial-tether --tag v0.3.1 serial-tether
```

**Pre-built binaries via curl** (no dependencies):
```sh
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/hulryung/serial-tether/releases/download/v0.3.1/serial-tether-installer.sh | sh
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
# Terminal 1 — daemon owns the port
tetherd -D /dev/tty.usbserial-XXXX -b 115200

# Terminal 2 — agent / scripted CLI
tether status
tether run "version" -u "# " --literal --timeout-ms 3000 --json
tether tail
```

## The one command an agent should reach for

```sh
tether --json run "$cmd" -u "$prompt" --literal --timeout-ms 5000
# → { matched, match, output (decoded text), truncated, duration_ms, ... }
# → exit 0 (ok) / 124 (timeout) / 2 (protocol) / 3 (connect) / 4 (device) / 5 (overflow) / 6 (lock)
```

Agent-friendly defaults are baked in: `--strip-ansi`, `--strip-echo`, `--max-output-bytes 8192`. The `--json` payload includes a decoded `output` field so an LLM never has to deal with base64.

## Protocol

[`docs/PROTOCOL.md`](docs/PROTOCOL.md) — JSON-RPC 2.0 over NDJSON. The same wire format works on UDS, Named Pipe, or TCP.

## Verifying the build

A virtual serial pair smoke test (no `socat` required, only Python 3 and the built binaries):

```sh
bash tools/smoke_test.sh
```

## Status

v0.2:
- ✅ `hello` / `attach` / `detach` / `send` / `expect` / `run` / `status`
- ✅ writer lock with `preempt` policy (queue / fail / force)
- ✅ `strip_ansi` / `strip_echo` / `max_output_bytes` (with truncation marker)
- ✅ standard exit codes; decoded `output` field in `--json`
- ✅ `sync` (send CR, wait until idle, surface a prompt candidate)
- ✅ ring-buffer fan-out with separate consumer / notify cursors per session

Not yet (v0.3+):
- TCP transport with token auth (work around with SSH socket forwarding for now)
- Windows Named Pipe backend
- `tether-tui` (interactive client with raw mode + escape sequences)
- 30-second session resume after disconnect
- `cancel` method

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or <http://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or <https://opensource.org/license/mit>)

at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in the work by you, as defined in the Apache-2.0 license, shall
be dual licensed as above, without any additional terms or conditions.
