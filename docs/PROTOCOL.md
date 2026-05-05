# Serial Tether Wire Protocol — v1 (draft)

The protocol spoken between `tetherd` (daemon) and clients (`tether`, the future `tether-tui`, ad-hoc user scripts).

## 0. Conceptual model

- The daemon owns a single serial port. All bytes flow through one ring buffer.
- Many clients can attach simultaneously. A "session" is *not* an isolated I/O stream; it is the bundle of:
  1. **identity** — who attached (used for logging and audit)
  2. **read cursor** — offset into the shared ring buffer
  3. **mode** — `rw` or `ro`
  4. **writer-lock candidacy** — the right to enter the critical section during a `run` transaction
- Serial output is fanned out to every attached session (each session has its own queue).

## 1. Transport

| Environment | Transport | Example path |
|---|---|---|
| Local Linux/macOS | Unix domain socket | `/run/tetherd.sock`, `~/.tether/sock` |
| Local Windows | Named Pipe | `\\.\pipe\tetherd` |
| Remote | TCP + token auth | `tcp://host:5557` |

Every transport is treated as a bidirectional byte stream. Authentication and authorization are handled at the transport layer; the wire format is identical above it.

## 2. Framing

- **NDJSON**: UTF-8, one JSON object per line, terminated by `\n` (LF).
- Newlines inside a payload are JSON-string-escaped (`\n`), so framing never collides with content.
- Recommended maximum line length: 1 MiB. The server closes the connection when this is exceeded.

## 3. Message shape — JSON-RPC 2.0

Three kinds of messages flow on the wire:

```jsonc
// Request — has an id, expects a response
{"jsonrpc":"2.0","id":1,"method":"<name>","params":{...}}

// Response — id matches the request
{"jsonrpc":"2.0","id":1,"result":{...}}
{"jsonrpc":"2.0","id":1,"error":{"code":N,"message":"...","data":{...}}}

// Notification — no id, no response (server-push, or client→server fire-and-forget)
{"jsonrpc":"2.0","method":"<name>","params":{...}}
```

- `id` is a client-issued integer or string. The server echoes it verbatim.
- Unknown *method* → `-32601` error.
- Unknown *param fields* → silently ignored (additive evolution).
- Unknown *notifications* → silently dropped.

## 4. Data-type conventions

- **session_id**: UUIDv7 string (time-sortable). Issued by the server.
- **seq**: byte offset into the ring buffer (u64). Monotonically increasing; resets to 0 when the daemon restarts.
- **bytes**: base64 (RFC 4648 standard). Text payloads in `send`/`expect` are encoded the same way for consistency.
  - For convenience some methods also accept `data_text` (UTF-8 only) as an alternative to base64.
- **timestamp**: RFC 3339 with microsecond precision, UTC. Filled in by the server.
- **timeout_ms**: u32 milliseconds. `0` or omitted does *not* mean "wait forever" — pass an explicit `null` for that.

## 5. Connection lifecycle

```
[client connects]
       ↓
   hello  ─────────────────→
       ←──────────────  hello result (server info)
       ↓
   attach  ────────────────→  (may be called multiple times to hold N sessions)
       ←──────────────  attach result (session_id)
       ↓
   send / expect / run / status / data notif / ...
       ↓
   detach (optional)  or  connection close
```

When a connection drops, every session it owned is automatically detached. Session IDs are retained for 30 seconds so the same client can reconnect with `attach { session_id }` to resume.

## 6. Method catalogue

### 6.1 `hello` (required, must be the first message)

```jsonc
// Request
{"id":1,"method":"hello","params":{
  "protocol_version":"1",
  "client":{
    "name":"tether",
    "version":"0.1.0",
    "kind":"agent"               // "human" | "agent" | "logger"
  },
  "auth_token":"..."             // required on TCP transport
}}

// Response
{"id":1,"result":{
  "server_version":"0.1.0",
  "protocol_version":"1",
  "device":{"path":"/dev/ttyUSB0","baud":115200,"data_bits":8,"parity":"none","stop_bits":1},
  "buffer":{"capacity_bytes":65536,"head_seq":12345,"tail_seq":3201}
}}
```

- A `protocol_version` major mismatch is rejected with `-32010 unsupported_protocol`.
- Calling any method before `hello` returns `-32011 not_initialized`.

### 6.2 `attach`

```jsonc
{"method":"attach","params":{
  "session_id": null,            // null for new; a string attempts to resume
  "mode":"rw",                   // "rw" | "ro"
  "replay":{"from":"now"},       // "start" | "now" | {"seq": N}
  "label":"agent-claude",        // display-only, optional
  "flow_control":"drop_oldest"   // "drop_oldest" | "disconnect"
}}

→ {"result":{
     "session_id":"01HV...",
     "cursor_seq":12345,
     "restored":false             // true when an existing session was resumed
   }}
```

Errors:
- `-32012 session_not_found` — the requested session id has expired or never existed.
- `-32013 mode_conflict` — refused by policy (e.g. policy of "one rw at a time" already satisfied).

### 6.3 `detach`

```jsonc
{"method":"detach","params":{"session_id":"..."}}
→ {"result":{}}
```

### 6.4 `send`

Non-atomic write. Does not acquire the writer lock; may interleave with sends from other sessions.

```jsonc
{"method":"send","params":{
  "session_id":"...",
  "data":"dmVyc2lvbgo=",         // base64
  // or
  "data_text":"version\n",       // UTF-8 plain text (one of the two)
  "eat_echo":false               // when true, advances the cursor past the echoed bytes
}}
→ {"result":{"bytes_written":8,"sent_at_seq":12389}}
```

`sent_at_seq` is the buffer's `head_seq` *immediately before the write*. The client can then call `expect(from_seq=sent_at_seq)` to match in a race-free way.

### 6.5 `expect`

```jsonc
{"method":"expect","params":{
  "session_id":"...",
  "pattern":"# $",
  "regex":true,                  // false → literal substring match
  "timeout_ms":3000,             // null → wait forever
  "strip_ansi":true,             // strip ANSI escapes before matching
  "strip_echo":"version\n",      // optional: strip the echoed command line from `before`
  "from_seq":12389,              // omitted → uses the session's current cursor
  "max_bytes":65536,             // accumulating beyond this raises buffer_overflow
  "max_output_bytes":8192        // truncate `before` to the trailing N bytes (matching window unaffected)
}}

// Success
→ {"result":{
     "matched":true,
     "match":"# ",
     "before":"dmVyc2lvbi4uLg==", // raw bytes up to the match (base64)
     "match_seq":12450,           // seq where the match began
     "end_seq":12452,             // seq just past the match (next cursor candidate)
     "truncated":false,           // true when before was capped by max_output_bytes
     "original_bytes":null        // pre-truncation length (when truncated)
   }}

// Failure (timeout)
→ {"error":{
     "code":-32001,
     "message":"timeout",
     "data":{"buffered":"...","buffered_seq_range":[12389,12440]}
   }}
```

On match (or timeout), the session's cursor advances to `end_seq`.

### 6.6 `run`

Atomic `send` + `expect`. Holds the writer lock for the duration.

```jsonc
{"method":"run","params":{
  "session_id":"...",
  "data_text":"version\n",
  "until":{"pattern":"# $","regex":true,"strip_ansi":true},
  "timeout_ms":3000,
  "preempt":"queue",             // "queue" | "fail" | "force"
  "strip_echo":true,             // remove the echoed command line from `before`
  "max_output_bytes":8192
}}

→ {"result":{
     "matched":true,
     "match":"# ",
     "before":"...",              // base64; same field name as expect for symmetry
     "match_seq":12450,
     "end_seq":12462,
     "truncated":false,
     "duration_ms":42
   }}
```

Meaning of `preempt`:
- `queue` (default) — if another session holds the lock, queue and run when it releases.
- `fail` — return `-32004 lock_contention` immediately.
- `force` — abort the current lock holder's `run` and seize the lock. Servers may restrict this to clients of `kind:"human"`.

### 6.7 `cancel`

```jsonc
{"method":"cancel","params":{"id":7}}
→ {"result":{"cancelled":true}}
```

The targeted request responds with `-32800 cancelled`. If the request has already completed or never existed, `cancelled:false`.

### 6.8 `status`

```jsonc
{"method":"status","params":{}}
→ {"result":{
     "device":{
       "path":"/dev/ttyUSB0",
       "baud":115200,
       "data_bits":8,
       "parity":"none",
       "stop_bits":1,
       "flow_control":"none",
       "connected":true
     },
     "buffer":{"head_seq":12345,"tail_seq":3201,"capacity":65536},
     "lock":{"holder_session_id":"01HV...","acquired_at":"2026-04-29T..."},
     "sessions":[
       {"id":"01HV...","label":"human-dkkang","mode":"rw","cursor_seq":12300,"lag_bytes":45},
       {"id":"01HW...","label":"agent-claude","mode":"rw","cursor_seq":12345,"lag_bytes":0}
     ]
   }}
```

`device.parity` is one of `"none" | "odd" | "even"`. `device.flow_control` is one of `"none" | "software" | "hardware"`.

### 6.9 `list_ports` (since v0.7)

Enumerate the serial ports the daemon's host machine knows about. Useful for picking a device to connect when you don't know the path. Returns an empty array on platforms or environments where enumeration is unavailable (a warning is logged server-side).

```jsonc
{"method":"list_ports","params":{}}
→ {"result":{"ports":[
     {
       "path":"/dev/ttyUSB0",
       "kind":"usb",                  // "usb" | "pci" | "bluetooth" | "unknown"
       "manufacturer":"FTDI",
       "product":"FT232R USB UART",
       "serial_number":"A50285BI",
       "vid":"0403",                  // lowercase 4-hex
       "pid":"6001"
     },
     {"path":"/dev/ttyS0","kind":"unknown"}
   ]}}
```

### 6.10 `set_device` (since v0.7)

Apply a partial update to the live serial settings. Every field is optional;
absent fields keep their current value. The change is applied to the open
`SerialStream` in place — the device handle is not dropped, in-flight reads/writes are not interrupted.

```jsonc
{"method":"set_device","params":{
   "baud":921600,                    // optional
   "data_bits":8,                    // optional, 5..=8
   "parity":"none",                  // optional, "none" | "odd" | "even"
   "stop_bits":1,                    // optional, 1 or 2
   "flow_control":"none"             // optional, "none" | "software" | "hardware"
 }}
→ {"result":{"device":{...}}}        // device shape from §6.8
```

A successful apply updates the daemon's stored config (so the same settings
survive an auto-reconnect) and broadcasts a `device` notification with
`kind:"config_changed"` to every attached client (§7.5).

Errors:

- `-32007 unsupported_serial_op` — the device's backend can't accept termios changes (e.g. PTYs, pipes).
- `-32008 invalid_serial_setting` — value out of range, unknown parity/flow string, or hardware refused.
- `-32602 invalid_params` — no fields supplied.
- `-32005 device_disconnected` — set during a reconnect attempt; the new settings are remembered and applied on the next successful open.

## 7. Server → client notifications

### 7.1 `data` — serial output

```jsonc
{"method":"data","params":{
  "session_id":"01HV...",        // from the receiving session's perspective
  "seq":12390,                   // first seq of this chunk
  "data":"Li4uIGRvbmUK"          // base64
}}
```

- Chunking is at the server's discretion (network/buffer boundaries). No semantic-unit guarantee.
- When the same data is fanned out to N sessions, each notification carries that session's cursor-aligned seq (which is identical to the global seq).

### 7.2 `lag` — backpressure caused data drops

```jsonc
{"method":"lag","params":{
  "session_id":"...",
  "dropped_bytes":4096,
  "dropped_range":[12100,12300],
  "resume_seq":12300
}}
```

Only emitted for sessions configured with `flow_control:"drop_oldest"`. Tells the client "you missed some data".

### 7.3 `lock`

```jsonc
{"method":"lock","params":{
  "kind":"acquired",             // "acquired" | "released" | "queued" | "preempted"
  "holder_session_id":"01HV...",
  "queue_depth":2
}}
```

### 7.4 `session`

```jsonc
{"method":"session","params":{
  "id":"01HV...",
  "kind":"detached",             // "attached" | "detached" | "preempted"
  "reason":"client_disconnect"
}}
```

### 7.5 `device`

```jsonc
{"method":"device","params":{
  "kind":"disconnected",         // "disconnected" | "reconnected" | "config_changed"
  "detail":"USB cable removed"
}}
```

If the device drops, in-flight `expect`/`run` requests fail with `-32005 device_disconnected`.

## 8. Error codes

| Code | Meaning |
|---|---|
| `-32700` | parse error (JSON-RPC standard) |
| `-32600` | invalid request |
| `-32601` | method not found |
| `-32602` | invalid params |
| `-32603` | internal error |
| `-32001` | timeout |
| `-32002` | session not attached |
| `-32003` | mode violation (write on `ro`) |
| `-32004` | lock contention (`preempt:fail`) |
| `-32005` | device disconnected |
| `-32006` | buffer overflow (`max_bytes` exceeded without a match) |
| `-32007` | unsupported serial operation (backend can't apply termios) |
| `-32008` | invalid serial setting (out-of-range / unknown value) |
| `-32010` | unsupported protocol |
| `-32011` | not initialized (called before `hello`) |
| `-32012` | session not found |
| `-32013` | mode conflict (policy refused) |
| `-32800` | cancelled |

## 9. Race / ordering guarantees

- Within a connection the server may process requests out of order (e.g. an `expect` blocks while another RPC runs in parallel). Responses are matched by `id`.
- The `seq` carried by `data` notifications is globally monotonic.
- `sent_at_seq` from a `send` response is `≤` the seq of any echo or response generated by that send. So calling `expect(from_seq=sent_at_seq)` immediately after `send` is race-free.

## 10. Evolution rules

- **Additive changes** (new fields, new methods, new notifications): bump the server's minor version, keep `protocol_version`.
- **Breaking changes**: bump the major `protocol_version`. The server may refuse to negotiate down at `hello`.
- Clients must ignore unknown response fields.
- The server ignores unknown request fields by default (a strict-validation mode is a separate option).

## 11. Debugging

- `tetherd --log-protocol <path>`: dumps every in/out message as `{ts, dir, conn_id, raw}` NDJSON.
- Talk to the daemon by hand:
  - **UDS** — `socat - UNIX-CONNECT:/run/tetherd.sock` (or `nc -U` on Linux).
  - **TCP** — `nc daemon-host 5557` (don't forget to send a valid `auth_token` in the first `hello`).
- A handy first message:
  `{"jsonrpc":"2.0","id":1,"method":"hello","params":{"protocol_version":"1","client":{"name":"manual","version":"0","kind":"human"},"auth_token":"<TOKEN_IF_TCP>"}}`

## 12. Open questions (to be resolved during v1.x)

- **Raw passthrough mode**: a separate connection mode where a human TUI streams keystrokes without wrapping each one in NDJSON. v0 finds plain `send` good enough; we'll add this if throughput becomes an issue.
- **Multiple devices**: should a single daemon host more than one serial port? v1 assumes a single device. When we add more, a `device_id` field can be added (additive).
- **Recording / replay**: a separate tool that replays session logs preserving timing (out of protocol scope).
- **TLS for TCP**: v0.4 ships TCP with token-based auth, plaintext on the wire. For untrusted networks, tunnel through SSH/WireGuard or wait for a future `--tls-cert/--tls-key` flag.
- **Compression**: optional gzip framing for TCP remote mode. Not needed locally; might help on high-latency links once TLS is in place.

---

**Status**: v1 draft. TCP transport (§1, §6.1) shipped in v0.4.0. v1.0 will be cut after PoC feedback is folded in.
