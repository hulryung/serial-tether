//! tether — non-interactive CLI for agents and shell scripts.
//!
//! Exit codes (uniform across subcommands):
//!   0   ok / matched
//!   1   usage error (clap)
//!   2   protocol / RPC error (other JSON-RPC errors)
//!   3   connection error (failed to dial the socket)
//!   4   device disconnected
//!   5   buffer overflow (`max_bytes` exceeded)
//!   6   lock contention (`preempt=fail` clash)
//!   7   unauthorized (TCP auth token missing or wrong)
//!   8   exec ran but the device shell reported no numeric status
//!       (non-POSIX console; see docs/EXEC_NONPOSIX_SHELLS.md)
//!   124 timeout (coreutils convention)

use std::io::IsTerminal;
use std::process::ExitCode;
use std::time::Duration;

use base64::Engine as _;
use clap::{Parser, Subcommand};
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpStream, UnixStream};
use tokio_util::codec::Framed;

use tether_protocol::message::{ResponsePayload, RpcId};
use tether_protocol::{
    AttachParams, ExpectParams, HelloParams, LockParams, Message, NdjsonCodec, Notification,
    Request, Response, RunParams, SendParams, UntilSpec,
};

// `tether pty` is Unix-only (openpty/termios/signals) — see the big comment
// block ahead of `pty_session` for the whole feature. Imports are cfg-gated
// too so a hypothetical non-unix build doesn't warn about unused ones.
#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
#[cfg(unix)]
use std::time::Instant;
#[cfg(unix)]
use tokio::io::{unix::AsyncFd, Interest};

/// Trailing footer for `tether --help`. Kept brief so the long-help output
/// still fits a single screen on most terminals; the URLs let humans (and
/// AI agents) follow up to the canonical docs without leaving the CLI.
const HELP_FOOTER: &str = "\
EXAMPLES:
    tether /dev/ttyUSB0                            tio-style standalone shell
    tether status                                  show daemon + device info
    tether run \"version\" -u \"# \" --literal         atomic scripted RPC
    tether -d board0 lines                         multi-device line control
    tether -d board0 config --baud 9600            live serial config change

COMMANDS BY CATEGORY:
    Interactive:    shell, tail, sync
    Scripted RPCs:  send, expect, run, exec
    Inspection:     status, list-devices, ports, config, agents
    Line control:   break, dtr, rts, lines
    Virtual port:   pty
    Lifecycle:      reconnect, disconnect, connect

LEARN MORE:
    Agents: run `tether agents` for a ready-to-use cookbook (no daemon needed).
    AI-agent setup (paste-and-go AGENTS.md / CLAUDE.md block):
        https://github.com/hulryung/serial-tether/blob/main/docs/AI_AGENT_GUIDE.md
    Cookbook the agent itself reads (canonical command + pitfalls):
        https://github.com/hulryung/serial-tether/blob/main/docs/AGENT_USAGE.md
    Wire protocol spec (JSON-RPC 2.0 / NDJSON, error codes, stability):
        https://github.com/hulryung/serial-tether/blob/main/docs/PROTOCOL.md
    Source / issues:
        https://github.com/hulryung/serial-tether
";

/// Self-contained agent cookbook printed by `tether agents`. Kept current with
/// the CLI so an agent can learn the tool from one local command, no network.
const AGENTS_GUIDE: &str = r#"# Using `tether` (serial console for agents)

`tether` talks to a serial device through the `tetherd` daemon. The daemon owns
the port; you attach as a client. You don't start or manage the daemon.

## Rules — read first
- ALWAYS target a device explicitly with `-d <id>`. Multi-device daemons require
  it; without it you get an `AmbiguousDevice` error (or, on a TTY, a menu).
- Export `TETHER_NONINTERACTIVE=1` so tether never blocks on a prompt, even if
  your harness allocates a PTY. (Same as the `--no-interactive` flag.)
- Prefer `exec` for shell commands: it returns just the output and the device's
  own exit status.

## Discover what's connected
    tether list-devices --json      # ids, paths, connected state
    tether status --json            # daemon + default device

## Run a command and capture only its output (device at a shell prompt)
    tether -d <id> exec "<cmd>"          # stdout = output; exit = device status
    tether -d <id> exec "<cmd>" --json   # {output, exit_code, duration_ms}
    # exits with the device command's status, like ssh:
    if tether -d <id> exec "test -f /etc/os-release"; then echo yes; fi
    # exec works on Linux shells (bash/busybox/dash) AND hush-enabled U-Boot.
    # It defaults to --newline cr, so U-Boot works out of the box; if a device
    # is registered shell=uboot the CR-only rule is enforced automatically.

## U-Boot / bootloader consoles
    # Easiest: register the device with shell=uboot (daemon or -D config):
    #   tetherd -D board=/dev/ttyUSB0,shell=uboot,prompt='=> '
    # then plain exec/run just work:
    tether -d board exec "mdio list"
    tether -d board run "printenv"          # -u defaults to the device prompt=
    # NEVER send crlf to U-Boot: CR runs the line, the trailing LF repeats it
    # (double execution). Use cr. exec on a shell=uboot device forces cr for you.
    # A U-Boot built WITHOUT the hush parser can't run exec at all — use run:
    tether -d board run "<cmd>" -u "=> " --newline cr

## Raw / non-shell console (no shell at all)
    tether -d <id> run "<cmd>" -u "<prompt-regex>" --newline cr --json
    tether -d <id> send "<bytes>" --newline cr
    tether -d <id> expect "<pattern>" --timeout-ms 5000

## Observe live output
    tether -d <id> tail                 # follow, like tail -f (Ctrl-C to stop)
    tether -d <id> tail --from start    # replay what's buffered, then follow

## Exit codes
    0 ok   2 protocol   3 connection   4 disconnected   5 buffer-overflow
    6 lock-contention   7 unauthorized   8 exec-ran-but-no-numeric-status
    124 timeout
    (exec additionally passes through the device command's own exit status;
     exit 8 / exit_code:null means the command ran but the shell isn't POSIX —
     see docs/EXEC_NONPOSIX_SHELLS.md)

## Remote daemon (over TCP)
    export TETHER_AUTH_TOKEN=<token>
    tether -s tcp://<host>:5557 -d <id> exec "<cmd>"

## Hand the port to a non-tether tool (virtual serial port)
    tether -d <id> pty -- <tool> {}          # private virtual port; {} = its path (also $TETHER_PTY);
                                             # port lives exactly as long as <tool>; exit code passes through
    tether -d <id> pty --link /tmp/my.pty    # or: print the path on stdout (flushed, after ready), run until killed
    tether -d <id> pty --read-only           # observation-only port
    tether -d <id> pty --lock -- <flasher> {}   # exclusive: other sessions' writes fail with lock_contention
Rules: one tool per virtual port (simultaneous readers split the byte stream);
baud set by the tool on the virtual port is a no-op (real rate = daemon config);
DTR/RTS can't traverse a PTY — reset the board first with
`tether -d <id> reset --seq "dtr=0 rts=1 wait=100 dtr=1 rts=0 wait=50 dtr=0"`.

## Pitfalls
- No `-d` on a multi-device daemon → error. Always pass `-d <id>`.
- Don't pass a raw /dev path in scripts (`tether /dev/...`): it spawns or
  redirects a daemon. Use `-d <id>` against the shared daemon instead.
- `exec` needs a shell (Linux shell or hush-enabled U-Boot). For a truly raw
  console (`shell=none`) exec refuses immediately — use run/send/expect.
- NEVER `--newline crlf` toward U-Boot — it double-executes every command.
  Use `cr` (or register the device `shell=uboot`, which enforces it).
- exit_code null (JSON) / exit 8: the command ran but the shell reported no
  numeric status — usually a non-POSIX console. See docs/EXEC_NONPOSIX_SHELLS.md.
- `output` (in --json) is decoded UTF-8 with ANSI/echo stripped — use it, not `before`.
"#;

#[derive(Parser, Debug)]
#[command(
    name = "tether",
    version,
    about = "tether — share any serial port with humans, scripts, and AI agents",
    long_about = "tether — share any serial port with humans, scripts, and AI agents.\n\n\
                  Most people never need to think about the daemon/client split:\n  \
                  tether /dev/ttyUSB0          tio-style — auto-spawn private daemon, drop to shell\n  \
                  tether                       attach to /tmp/tetherd.sock (long-lived daemon)\n  \
                  tether status                inspect daemon + device(s)\n\n\
                  Pass <PATH> as the first arg (or `-D <PATH>`) to run in standalone mode\n\
                  for a single throwaway session.",
    after_long_help = HELP_FOOTER
)]
struct Cli {
    /// Daemon endpoint. Either a UDS path (e.g. /tmp/tetherd.sock) or
    /// `tcp://host:port` / `tcp:host:port` for a remote daemon.
    ///
    /// Default: `/tmp/tetherd.sock`, or `/tmp/tetherd-<NAME>.sock` if
    /// `--name` is set.
    #[arg(short = 's', long, global = true, conflicts_with = "name", help_heading = "Endpoint")]
    socket: Option<String>,

    /// Connect to the named daemon at `/tmp/tetherd-<NAME>.sock`.
    ///
    /// Convenience alias when `tetherd` was started with the same `--name`.
    /// Mutually exclusive with `--socket` (use `-s tcp://...` for TCP).
    #[arg(long, global = true, value_name = "NAME", help_heading = "Endpoint")]
    name: Option<String>,

    /// Target device id within the daemon. Required when the daemon serves
    /// more than one device (otherwise the daemon answers `AmbiguousDevice`).
    /// Single-device daemons may omit this — it falls through to the only
    /// device. Distinct from `--name` (which selects which *daemon*).
    #[arg(short = 'd', long, global = true, value_name = "ID", help_heading = "Device target")]
    device_id: Option<String>,

    /// Emit raw JSON output instead of human-readable form.
    #[arg(long, global = true, help_heading = "Output")]
    json: bool,

    /// Auto-reconnect + retry once on a `device_disconnected` reply.
    ///
    /// Useful for long-running scripts that should ride out a USB
    /// hiccup. Off by default.
    #[arg(long, global = true, help_heading = "Output")]
    auto_reconnect: bool,

    /// Never prompt: disable the interactive port / device pickers and fail
    /// with the usual error instead. For agents and automation that may run
    /// under a PTY. Setting `TETHER_NONINTERACTIVE=1` does the same.
    #[arg(long, global = true, help_heading = "Output")]
    no_interactive: bool,

    /// Auth token for TCP transport (alternative to TETHER_AUTH_TOKEN env var).
    #[arg(long, global = true, env = "TETHER_AUTH_TOKEN", help_heading = "TCP auth")]
    auth_token: Option<String>,

    /// Standalone mode: also start a private `tetherd` for this device,
    /// run the requested command (or shell), then shut the daemon down
    /// when the client exits. Same UX as `tio /dev/ttyUSB0`.
    /// Cannot be combined with `-s tcp://...` or an explicit `-s` socket.
    ///
    /// Accepts the same inline settings as `tetherd -D`, including console
    /// personality: `-D /dev/ttyUSB0,shell=uboot,prompt='=> '` makes `exec`
    /// use CR-only framing and `run`'s `-u` default to the prompt.
    #[arg(short = 'D', long, global = true, value_name = "DEVICE", help_heading = "Standalone mode")]
    device: Option<String>,

    /// Baud rate for standalone mode (only used when `-D` is given).
    /// Not `global` so the `config` subcommand can have its own optional
    /// `--baud` flag for `set_device`.
    #[arg(short = 'b', long, default_value_t = 115200, help_heading = "Standalone mode")]
    baud: u32,

    /// Also expose the embedded daemon over TCP while you're in standalone mode.
    ///
    /// Lets a remote client (e.g. an AI agent in a Lima VM) attach to the
    /// same device for the duration of your local session. Bare `--tcp`
    /// listens on 0.0.0.0:5557; for a custom bind use the equals form:
    /// `--tcp=127.0.0.1:5557` (the equals is required so `--tcp` followed
    /// by a subcommand name like `status` isn't mis-parsed as the value).
    /// Has no effect without `-D` (or the bare-path shorthand).
    /// Auth token: pass `--auth-token`, or one is auto-generated and printed
    /// on stderr before the shell starts. **The TCP listener dies when you
    /// quit** — remote clients see their connection drop.
    #[arg(
        long,
        global = true,
        value_name = "HOST:PORT",
        num_args = 0..=1,
        default_missing_value = "0.0.0.0:5557",
        require_equals = true,
        help_heading = "Standalone mode"
    )]
    tcp: Option<String>,

    /// If no subcommand is given, drops into the interactive shell.
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand, Debug, Clone)]
enum Cmd {
    // ──────── Interactive ────────
    /// Interactive raw-mode shell (Ctrl-A Q to quit; Ctrl-A ? for help).
    ///
    /// Forwards stdin to the device; renders live device output to stdout.
    /// Ctrl-A is the escape prefix:
    ///   Ctrl-A Q      quit
    ///   Ctrl-A C      show live serial config
    ///   Ctrl-A V      list available serial ports
    ///   Ctrl-A B      send BREAK pulse
    ///   Ctrl-A D      toggle DTR
    ///   Ctrl-A R      toggle RTS
    ///   Ctrl-A L      show modem status (CTS / DSR / RI / DCD)
    ///   Ctrl-A ?      help
    ///   Ctrl-A Ctrl-A send a literal Ctrl-A to the device
    Shell {
        /// Replay buffer position when attaching.
        #[arg(long, default_value = "now", value_parser = ["start", "now"])]
        from: String,
    },
    /// Stream live device output to stdout, like `tail -f` (Ctrl-C to stop).
    ///
    /// Read-only: doesn't take the writer lock, so it runs alongside `send` /
    /// `exec` / `shell` and other tails. Device data goes to stdout (redirect
    /// it for a clean log); connect/disconnect events go to stderr.
    Tail {
        /// Where to start: `now` = only new output from here; `start` = replay
        /// what's still in the daemon's buffer first, then follow.
        #[arg(long, default_value = "now", value_parser = ["now", "start"])]
        from: String,
    },
    /// Send CR and wait until the device goes idle; print the last line as a
    /// prompt candidate.
    Sync {
        /// Treat the device as idle once this many ms pass with no new output.
        #[arg(long, default_value_t = 300)]
        idle_ms: u32,
        /// Give up after this many ms if the device never goes idle.
        #[arg(long, default_value_t = 2000)]
        timeout_ms: u32,
    },

    // ──────── Scripted RPCs ────────
    /// Send data to the device. Does not wait for a response.
    Send {
        /// Bytes to send (see --base64 / --newline).
        data: String,
        /// Treat `data` as base64.
        #[arg(long)]
        base64: bool,
        /// Append a line terminator to the data before sending.
        #[arg(long, default_value = "none", value_parser = ["none", "lf", "cr", "crlf"])]
        newline: String,
    },
    /// Wait until a pattern appears in the device output.
    Expect {
        /// Pattern to wait for (regex by default; use --literal for a fixed string).
        pattern: String,
        /// Give up waiting for the pattern after this many ms.
        #[arg(long, default_value_t = 3000)]
        timeout_ms: u32,
        /// Match `pattern` as a literal string instead of a regex.
        #[arg(long)]
        literal: bool,
        /// Strip ANSI escape sequences before matching.
        #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
        strip_ansi: bool,
        /// Cap the captured pre-match output; keep only the last N bytes.
        #[arg(long, default_value_t = 8192)]
        max_output_bytes: u64,
    },
    /// Atomic send + expect executed on the server (holds the writer lock).
    Run {
        /// Data/command to send before waiting for --until.
        data: String,
        /// Pattern that ends the wait (regex by default; see --literal).
        ///
        /// Optional when the target device has a `prompt=` configured (see
        /// `-D`/daemon device config): that prompt regex is used as the
        /// default. Without either, `run` errors asking for `-u`.
        #[arg(short = 'u', long)]
        until: Option<String>,
        /// Give up waiting for --until after this many ms.
        #[arg(long, default_value_t = 3000)]
        timeout_ms: u32,
        /// Match --until as a literal string instead of a regex.
        #[arg(long)]
        literal: bool,
        /// Strip ANSI escape sequences before matching.
        #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
        strip_ansi: bool,
        /// Strip the echoed command line from the response.
        #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
        strip_echo: bool,
        /// Cap the captured output; keep only the last N bytes.
        #[arg(long, default_value_t = 8192)]
        max_output_bytes: u64,
        /// Behaviour when the writer lock is contended.
        #[arg(long, default_value = "queue", value_parser = ["queue", "fail", "force"])]
        preempt: String,
        /// Append a line terminator to the data before sending.
        /// Most embedded shells (U-Boot, busybox, Linux login) want `crlf`,
        /// `lf`, or (U-Boot) `cr`. Defaults to the device's configured
        /// `newline=`, else `none`. Never use `crlf` toward U-Boot — its CLI
        /// double-executes (CR runs the line, the trailing LF repeats it).
        #[arg(long, value_parser = ["none", "lf", "cr", "crlf"])]
        newline: Option<String>,
    },
    /// Run a shell command on the device and capture just its output.
    ///
    /// Wraps `<data>` so the device shell brackets the output with unique
    /// markers and reports the command's exit status, then returns only the
    /// bytes the command produced — no prompt-parsing or BEGIN/END scaffolding
    /// on your side, and the echoed command line is dropped even when the
    /// device terminal wraps it. The captured output goes to stdout; `tether
    /// exec` then exits with the *device command's* status, like `ssh`. Pair
    /// with `--json` to get `{output, exit_code, duration_ms}`.
    ///
    /// Assumes a POSIX-ish shell on the device (busybox, dash, bash, U-Boot
    /// hush) sitting at a prompt. Not for raw/non-shell consoles — use
    /// `send` + `expect` / `run` there.
    Exec {
        /// The command line to run on the device shell.
        data: String,
        /// Give up if the end-marker hasn't appeared after this many ms.
        #[arg(long, default_value_t = 5000)]
        timeout_ms: u32,
        /// Strip ANSI escapes from the captured output.
        #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
        strip_ansi: bool,
        /// Cap the captured output; keep only the last N bytes.
        #[arg(long, default_value_t = 65536)]
        max_output_bytes: u64,
        /// Behaviour when the writer lock is contended.
        #[arg(long, default_value = "queue", value_parser = ["queue", "fail", "force"])]
        preempt: String,
        /// Line terminator used to submit the command. Serial consoles usually
        /// want `cr` (the default). A device with `shell=uboot` always forces
        /// `cr` regardless of this flag — `crlf`/`lf` double-execute U-Boot's
        /// CLI. Otherwise resolves to the flag, then the device's `newline=`,
        /// then `cr`.
        #[arg(long, value_parser = ["lf", "cr", "crlf"])]
        newline: Option<String>,
    },

    // ──────── Inspection ────────
    /// Show daemon status.
    Status,
    /// List all devices managed by the daemon.
    ///
    /// AI-agent tip: pair with `--json`. Output also tells you which id is
    /// the default (used when `--device` is omitted on single-device daemons).
    #[command(name = "list-devices")]
    ListDevices,
    /// List serial ports the daemon machine knows about.
    ///
    /// AI-agent tip: pass `--json` to get a stable schema. Returns an empty
    /// `ports` array on platforms that can't enumerate (e.g. restricted
    /// containers).
    Ports,
    /// Print a ready-to-use cookbook for AI agents and exit (no daemon needed).
    ///
    /// Tell an agent to run `tether agents` first — it prints the canonical
    /// commands (`-d <id> exec …`), the non-interactive contract, exit codes,
    /// and the common pitfalls. The output is also fine to paste into an
    /// `AGENTS.md` / `CLAUDE.md` block.
    Agents,
    /// Show or change the live serial configuration.
    ///
    /// With no flags: prints the current device settings.
    /// With one or more of --baud/--data-bits/--parity/--stop-bits/--flow:
    /// applies the partial update via `set_device` and prints the new state.
    Config {
        /// New baud rate (e.g. 9600, 115200, 921600).
        #[arg(long)]
        baud: Option<u32>,
        /// 5, 6, 7, or 8.
        #[arg(long, value_parser = clap::value_parser!(u8).range(5..=8))]
        data_bits: Option<u8>,
        /// none | odd | even.
        #[arg(long, value_parser = ["none", "odd", "even"])]
        parity: Option<String>,
        /// 1 or 2.
        #[arg(long, value_parser = clap::value_parser!(u8).range(1..=2))]
        stop_bits: Option<u8>,
        /// none | software | hardware.
        #[arg(long = "flow", value_parser = ["none", "software", "hardware"])]
        flow_control: Option<String>,
    },

    // ──────── Line control (tio-parity) ────────
    /// Send a serial BREAK pulse to the device.
    Break {
        /// Break duration. Default 250ms (matches tio).
        #[arg(long, default_value_t = 250)]
        duration_ms: u32,
    },
    /// Drive the DTR (Data Terminal Ready) output line.
    Dtr {
        /// `on` asserts the line; `off` deasserts.
        #[arg(value_parser = ["on", "off"])]
        state: String,
    },
    /// Drive the RTS (Request To Send) output line.
    Rts {
        /// `on` asserts the line; `off` deasserts.
        #[arg(value_parser = ["on", "off"])]
        state: String,
    },
    /// Read the four input modem status lines (CTS / DSR / RI / DCD).
    Lines,
    /// Reset the board by driving a DTR/RTS sequence on the *real* port.
    ///
    /// Use this for flashing through a shared/virtual port: a PTY can't carry
    /// modem control, so a flashing tool's DTR/RTS auto-reset won't pass through
    /// `pty=`. Run `reset` against the device to drive the reset here, then
    /// flash with the tool's no-auto-reset option (e.g. esptool
    /// `--before no_reset`) pointed at `/tmp/tether-<id>.pty`.
    ///
    /// Provide a preset (`--esp32`) or an explicit `--seq`. Line state `1` =
    /// asserted, `0` = deasserted; `wait=<ms>` sleeps. Example (esptool classic):
    ///   tether -d esp reset --seq "dtr=0 rts=1 wait=100 dtr=1 rts=0 wait=50 dtr=0"
    Reset {
        /// esptool "classic" auto-reset sequence (leaves the chip in download mode).
        #[arg(long)]
        esp32: bool,
        /// Custom sequence: space/comma-separated steps `dtr=0|1`, `rts=0|1`,
        /// `wait=<ms>`, executed in order on the real port.
        #[arg(long)]
        seq: Option<String>,
    },

    // ──────── Virtual port (client-side) ────────
    /// Create an on-demand virtual serial port on THIS machine, bridged to
    /// the device through the daemon.
    ///
    /// `openpty()`s a fresh master/slave pair here — this also works against
    /// a remote daemon over `-s tcp://...` — and publishes the slave under a
    /// symlink: default `/tmp/tether-<id>-<n>.pty`, or `--link PATH`. Point
    /// any serial tool (minicom, screen, pyserial, a vendor flasher) at that
    /// path; it behaves like a real device node while `tether` relays bytes
    /// to/from the daemon underneath. Distinct from the daemon's own `pty=`
    /// device option (see `tetherd --help`) — this one lives in the client
    /// and needs no daemon reconfiguration. With the global `--json`,
    /// readiness is a single NDJSON line instead of the bare link path.
    ///
    /// Baud rate set by the attached tool is a no-op: a PTY has no UART, so
    /// the real transfer rate is whatever the daemon's device config says.
    /// For flashing: pass `--lock` to hold the writer lock for the session,
    /// and run `tether reset` first if the flasher needs DTR/RTS auto-reset
    /// (a PTY can't carry modem control).
    ///
    /// One tool per virtual port — run a separate `tether pty` (its own
    /// `--link`) for each concurrent tool.
    Pty {
        /// Publish the slave under this path instead of the default
        /// `/tmp/tether-<id>-<n>.pty`.
        #[arg(long, value_name = "PATH")]
        link: Option<String>,
        /// Attach read-only: bytes typed into the virtual port are counted
        /// and dropped instead of reaching the device.
        #[arg(long)]
        read_only: bool,
        /// Hold the device's writer lock for this session (see `tether run
        /// --preempt`). Needs a `tetherd` new enough to know the `lock` RPC.
        #[arg(long)]
        lock: bool,
        /// Spawn CMD once the port is ready; every `{}` in its arguments is
        /// replaced by the link path (appended as a final argument if none
        /// contains `{}`). Bridging continues while CMD runs; `tether pty`
        /// exits with CMD's status when it exits.
        #[arg(last = true)]
        exec: Vec<String>,
    },

    // ──────── Lifecycle ────────
    /// Drop and reopen the serial device (kick a wedged bus).
    ///
    /// The daemon closes the open port and immediately tries to reopen.
    /// Useful when `status` shows `connected:true` but commands hang —
    /// usually a USB driver hiccup or a half-completed board reset.
    Reconnect {
        /// Don't wait for the device to come back online.
        #[arg(long)]
        nowait: bool,
        /// How long to wait for the device to reopen.
        #[arg(long, default_value_t = 5000)]
        timeout_ms: u32,
    },
    /// Explicitly close the port; auto-reconnect pauses until `connect`.
    ///
    /// Use this to hand the device temporarily to another tool (`flashrom`,
    /// vendor flasher, etc.) without killing the daemon. The daemon
    /// remembers the explicit-disconnect state and won't auto-reopen.
    Disconnect,
    /// Reopen a port closed by `tether disconnect`.
    ///
    /// No-op if the device wasn't explicitly disconnected. Returns the
    /// fresh device info; pair with `--json` to see the new `connected`
    /// flag.
    Connect,
}

/// Tio-like quick-start sugar: if the user runs `tether /dev/ttyUSB0`
/// (path as the first positional arg, no daemon involvement), rewrite the
/// argv to `tether -D /dev/ttyUSB0` so the existing standalone-mode code
/// path takes over. The goal is for a first-time user to never have to
/// know about the daemon/client split.
///
/// Detection rule (intentionally narrow to avoid surprising existing
/// users):
///   - We look at `args[1]` only. If a user mixes leading flags with a
///     path positional later, they should still pass `-D` explicitly.
///   - The token must contain `/` or start with `~` to count as a path.
///     Plain identifiers (`status`, `dtr`, `board0`, …) fall through to
///     normal subcommand parsing.
fn rewrite_argv_for_path_shorthand(args: &mut Vec<String>) {
    if args.len() < 2 {
        return;
    }
    let first = &args[1];
    if first.starts_with('-') {
        return; // user gave a flag first; don't second-guess
    }
    let looks_like_path = first.contains('/') || first.starts_with('~');
    if !looks_like_path {
        return;
    }
    // Don't double-up if the user somehow already passed `-D` later.
    if args.iter().skip(2).any(|a| a == "-D" || a == "--device") {
        return;
    }
    args.insert(1, "-D".to_string());
}

fn main() -> ExitCode {
    let mut argv: Vec<String> = std::env::args().collect();
    rewrite_argv_for_path_shorthand(&mut argv);
    let cli = Cli::parse_from(argv);
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    let result = runtime.block_on(run(cli));
    match result {
        Ok(()) => ExitCode::SUCCESS,
        // `exec` mirrors the device command's status. No extra message — the
        // captured output already went to stdout.
        Err(CliError::RemoteExit(code)) => ExitCode::from(code),
        Err(CliError::Timeout(_)) => ExitCode::from(124),
        Err(CliError::DeviceDisconnected) => ExitCode::from(4),
        Err(CliError::BufferOverflow) => ExitCode::from(5),
        Err(CliError::LockContention) => ExitCode::from(6),
        Err(CliError::Unauthorized(msg)) => {
            eprintln!("tether: unauthorized: {msg}");
            ExitCode::from(7)
        }
        // The command ran but the device shell didn't hand back a numeric
        // status. `print_exec_result` already printed the output and the
        // one-line hint; just carry the distinct exit code.
        Err(CliError::UnknownExecStatus) => ExitCode::from(8),
        Err(CliError::Protocol(msg)) => {
            eprintln!("protocol error: {msg}");
            ExitCode::from(2)
        }
        Err(CliError::Connection(msg)) => {
            // The message itself is already self-contained (multi-line) for
            // the common cases — print it verbatim instead of mashing it on
            // one line behind a "connection error:" prefix.
            if msg.contains('\n') {
                eprintln!("{msg}");
            } else {
                eprintln!("tether: connection error: {msg}");
            }
            ExitCode::from(3)
        }
    }
}

#[derive(Debug)]
enum CliError {
    /// A `run`/`expect` RPC timed out. Carries the daemon's decoded pre-match
    /// buffer when available (from the error `data.buffered` field), so `exec`
    /// can diagnose *why* the end marker never parsed instead of blaming a
    /// missing prompt.
    Timeout(Option<String>),
    /// `exec` only: the device command ran but returned a non-zero status,
    /// which `tether exec` mirrors as its own exit code (ssh-style).
    RemoteExit(u8),
    /// `exec` only: the command ran and its output was captured, but the
    /// device shell didn't report a numeric status (non-POSIX console left
    /// the `$?` token literal). Distinct from a device failure.
    UnknownExecStatus,
    DeviceDisconnected,
    BufferOverflow,
    LockContention,
    Unauthorized(String),
    Protocol(String),
    Connection(String),
}

impl From<std::io::Error> for CliError {
    fn from(e: std::io::Error) -> Self {
        CliError::Connection(e.to_string())
    }
}

/// Parse `-s` value into a connection target. Recognized forms:
///   /path/to/sock       → UDS
///   tcp://host:port     → TCP
///   tcp:host:port       → TCP
fn endpoint_kind(s: &str) -> Endpoint<'_> {
    if let Some(addr) = s.strip_prefix("tcp://") {
        Endpoint::Tcp(addr)
    } else if let Some(addr) = s.strip_prefix("tcp:") {
        Endpoint::Tcp(addr)
    } else {
        Endpoint::Uds(s)
    }
}

enum Endpoint<'a> {
    Uds(&'a str),
    Tcp(&'a str),
}

/// A serial port the convenience picker can offer.
struct PortChoice {
    path: String,
    label: String,
}

/// Discover USB serial ports worth offering in the auto-start picker.
///
/// Filters the raw `available_ports()` list down to real USB serial adapters
/// (drops virtual junk like `*-Bluetooth-Incoming-Port` / `*.debug-console`).
/// On macOS each device shows up twice — `/dev/cu.*` (call-out, the right node
/// to open) and `/dev/tty.*` (dial-in) — so the dial-in duplicates are dropped.
/// The `tty.` test is the macOS dial-in pattern (note the dot); Linux nodes
/// like `/dev/ttyUSB0` have no dot and are kept.
fn discover_usb_ports() -> Vec<PortChoice> {
    let ports = match tokio_serial::available_ports() {
        Ok(p) => p,
        Err(_) => return Vec::new(),
    };
    select_port_choices(ports.into_iter().map(|p| {
        let desc = match &p.port_type {
            tokio_serial::SerialPortType::UsbPort(info) => Some(
                info.product
                    .clone()
                    .or_else(|| info.manufacturer.clone())
                    .unwrap_or_default(),
            ),
            _ => None, // not USB → desc None signals "skip"
        };
        (p.port_name, desc)
    }))
}

/// Pure filtering/labeling rule behind [`discover_usb_ports`], split out so it
/// can be unit-tested without real hardware. Input is `(path, usb_desc)` where
/// `usb_desc` is `Some` only for USB serial adapters. Keeps USB ports, drops
/// macOS dial-in duplicates (`/dev/tty.*`, note the dot — Linux `/dev/ttyUSB0`
/// has none and is kept), and builds a display label.
fn select_port_choices<I>(ports: I) -> Vec<PortChoice>
where
    I: IntoIterator<Item = (String, Option<String>)>,
{
    ports
        .into_iter()
        .filter_map(|(path, desc)| {
            let desc = desc?; // None → not USB → skip
            if path.starts_with("/dev/tty.") {
                return None;
            }
            let label = if desc.is_empty() {
                path.clone()
            } else {
                format!("{path}  ({desc})")
            };
            Some(PortChoice { path, label })
        })
        .collect()
}

/// Print a numbered menu to stderr and read a 1-based choice from stdin.
/// Returns the selected index, or `None` when not interactive, on EOF, or on an
/// invalid/blank-with-no-default entry. Pressing Enter selects the first item.
fn pick_index(title: &str, labels: &[String], interactive: bool) -> Option<usize> {
    use std::io::Write as _;
    if labels.is_empty() || !interactive {
        return None;
    }
    let mut err = std::io::stderr();
    let _ = writeln!(err, "{title}");
    for (i, l) in labels.iter().enumerate() {
        let _ = writeln!(err, "  {}) {l}", i + 1);
    }
    let _ = write!(err, "Select [1-{}] (Enter=1): ", labels.len());
    let _ = err.flush();
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).ok()? == 0 {
        return None;
    }
    let s = line.trim();
    if s.is_empty() {
        return Some(0);
    }
    match s.parse::<usize>() {
        Ok(n) if (1..=labels.len()).contains(&n) => Some(n - 1),
        _ => {
            let _ = writeln!(err, "tether: invalid selection — aborting.");
            None
        }
    }
}

/// Whether `TETHER_NONINTERACTIVE` is set to a truthy value. Read manually
/// (not via clap's `env`) so the natural `TETHER_NONINTERACTIVE=1` works —
/// clap's bool-from-env only accepts `true`/`false`.
fn env_forces_noninteractive() -> bool {
    match std::env::var("TETHER_NONINTERACTIVE") {
        Ok(v) => {
            let v = v.trim().to_ascii_lowercase();
            !matches!(v.as_str(), "" | "0" | "false" | "no" | "off")
        }
        Err(_) => false,
    }
}

/// True when it's safe to show an interactive prompt: stdin and stderr are both
/// a terminal AND the caller hasn't opted out (`--no-interactive` /
/// `TETHER_NONINTERACTIVE`). Scripts, pipes, and agents stay non-prompting.
fn interactive_allowed(no_interactive: bool) -> bool {
    !no_interactive
        && !env_forces_noninteractive()
        && std::io::stdin().is_terminal()
        && std::io::stderr().is_terminal()
}

/// Quick liveness probe: can we open the daemon's UDS? A stale socket file
/// (crashed daemon) refuses the connection and reports `false`.
async fn uds_daemon_alive(path: &str) -> bool {
    UnixStream::connect(path).await.is_ok()
}

/// Commands that don't target a specific device (so the multi-device picker
/// shouldn't fire for them).
fn command_needs_device(cmd: &Cmd) -> bool {
    !matches!(cmd, Cmd::Status | Cmd::ListDevices | Cmd::Ports | Cmd::Agents)
}

/// Convenience picker for a daemon that manages more than one device when the
/// user didn't pass `-d`. Interactive terminals get a menu; everything else
/// returns `None` so the per-command `AmbiguousDevice` error still fires (and
/// scripts keep working). A single-device daemon also returns `None` — the
/// daemon auto-selects the only device server-side.
async fn maybe_pick_device<S>(
    framed: &mut Framed<S, NdjsonCodec>,
    next_id: &mut i64,
    interactive: bool,
) -> Result<Option<String>, CliError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    if !interactive {
        return Ok(None);
    }
    let v = call(framed, next_id, "list_devices", json!({})).await?;
    let devices = v
        .get("devices")
        .and_then(|d| d.as_array())
        .cloned()
        .unwrap_or_default();
    if devices.len() <= 1 {
        return Ok(None);
    }
    let labels: Vec<String> = devices
        .iter()
        .map(|d| {
            let id = d.get("id").and_then(|s| s.as_str()).unwrap_or("?");
            let path = d.get("path").and_then(|s| s.as_str()).unwrap_or("");
            let connected = d.get("connected").and_then(|b| b.as_bool()).unwrap_or(false);
            let mark = if connected { "" } else { "  (disconnected)" };
            format!("{id}  [{path}]{mark}")
        })
        .collect();
    match pick_index(
        "tether: this daemon manages several devices. Pick one:",
        &labels,
        interactive,
    ) {
        Some(i) => Ok(devices[i].get("id").and_then(|s| s.as_str()).map(String::from)),
        None => Ok(None),
    }
}

async fn run(mut cli: Cli) -> Result<(), CliError> {
    // `tether agents` is pure local output — no daemon, no device. Handle it
    // before any connection logic so an agent can read it with nothing set up.
    if matches!(cli.cmd, Some(Cmd::Agents)) {
        print!("{AGENTS_GUIDE}");
        return Ok(());
    }

    // Whether the user pinned an endpoint. When they didn't (bare `tether`),
    // we may auto-start a session daemon below instead of erroring out.
    let explicit_endpoint = cli.socket.is_some() || cli.name.is_some();

    // Resolve --socket vs --name vs default into a single canonical endpoint
    // string, then keep using cli.socket as the source of truth (so
    // standalone mode can still rewrite it after spawning the daemon).
    let resolved = match (&cli.socket, &cli.name) {
        (Some(s), _) => s.clone(),
        (None, Some(n)) => format!("/tmp/tetherd-{n}.sock"),
        (None, None) => "/tmp/tetherd.sock".to_string(),
    };
    cli.socket = Some(resolved);

    // Convenience: bare `tether` (default endpoint, no device pinned) with no
    // daemon running. Instead of erroring, pick a serial port interactively and
    // fall through to standalone mode — an ephemeral daemon that lives only for
    // this session, like `tio /dev/ttyUSB0`. Only fires in an interactive
    // terminal; scripts/pipes keep the original "no daemon" error so automation
    // isn't surprised by a prompt.
    let interactive = interactive_allowed(cli.no_interactive);
    if !explicit_endpoint && cli.device.is_none() && cli.device_id.is_none() && interactive {
        if let Endpoint::Uds(path) = endpoint_kind(cli.socket.as_deref().unwrap()) {
            if !uds_daemon_alive(path).await {
                let ports = discover_usb_ports();
                match ports.len() {
                    0 => {
                        return Err(CliError::Connection(
                            "tether: no USB serial ports found. Plug one in, or name it explicitly:\n  \
                             tether /dev/<port>            (one-off session)\n  \
                             tetherd -D /dev/<port> -b ...  (long-lived daemon)"
                                .into(),
                        ));
                    }
                    1 => {
                        eprintln!(
                            "tether: no daemon running — opening {} for this session.",
                            ports[0].path
                        );
                        cli.device = Some(ports[0].path.clone());
                    }
                    _ => {
                        let labels: Vec<String> = ports.iter().map(|p| p.label.clone()).collect();
                        match pick_index(
                            "tether: no daemon running. Pick a serial port to open (this session only):",
                            &labels,
                            interactive,
                        ) {
                            Some(i) => {
                                eprintln!("tether: opening {} for this session.", ports[i].path);
                                cli.device = Some(ports[i].path.clone());
                            }
                            None => {
                                return Err(CliError::Connection(
                                    "tether: several serial ports found — pick one explicitly:\n  \
                                     tether /dev/<port> ..."
                                        .into(),
                                ));
                            }
                        }
                    }
                }
            }
        }
    }

    // If the user (or AI agent) asked for standalone mode but a daemon is
    // already managing this exact device path, attach to that daemon as a
    // client instead of spawning a new one. Avoids the "two processes both
    // open /dev/ttyUSB0" failure where the existing user's session gets
    // garbled or kicked.
    if let Some(device) = cli.device.as_deref() {
        if let Some(found) = find_daemon_managing_device(device).await {
            eprintln!(
                "tether: device {device} is already managed by daemon at {} (id: {})",
                found.socket, found.device_id
            );
            eprintln!(
                "tether: attaching as a client — no new daemon spawned, the existing session keeps running."
            );
            cli.socket = Some(found.socket);
            if cli.device_id.is_none() {
                cli.device_id = Some(found.device_id);
            }
            cli.device = None;
        }
    }

    // Standalone mode: spawn our own tetherd, then continue as a normal
    // client against its ephemeral UDS. The guard kills the child when
    // we exit, regardless of how (clean exit, error, panic).
    let _daemon_guard = if let Some(device) = cli.device.clone() {
        if matches!(endpoint_kind(cli.socket.as_deref().unwrap()), Endpoint::Tcp(_)) {
            return Err(CliError::Connection(
                "tether: -D and -s tcp://... are mutually exclusive (standalone mode is local UDS only)".into(),
            ));
        }
        Some(spawn_embedded_daemon(&device, cli.baud, &mut cli).await?)
    } else {
        None
    };

    match endpoint_kind(cli.socket.as_deref().unwrap()) {
        Endpoint::Uds(path) => {
            let stream = UnixStream::connect(path)
                .await
                .map_err(|e| make_uds_connect_error(path, e))?;
            let framed = Framed::new(stream, NdjsonCodec::new());
            run_with_stream(framed, cli).await
        }
        Endpoint::Tcp(addr) => {
            let stream = TcpStream::connect(addr)
                .await
                .map_err(|e| make_tcp_connect_error(addr, e))?;
            let _ = stream.set_nodelay(true);
            let framed = Framed::new(stream, NdjsonCodec::new());
            run_with_stream(framed, cli).await
        }
    }
}

/// Result of `find_daemon_managing_device`: the socket of an existing
/// daemon plus the daemon-side device id matching the requested OS path.
#[derive(Debug)]
struct ExistingDaemon {
    socket: String,
    device_id: String,
}

/// Scan local UDS sockets and return the daemon that already manages the
/// given OS device path, if any. Used to short-circuit standalone-mode
/// (`-D <PATH>`) when a long-lived `tetherd` is already running for that
/// device — spawning a second daemon there would have both processes
/// fighting for the port, garbling the existing user's session.
///
/// Implementation:
///   - Glob `/tmp/tetherd*.sock`.
///   - Probe each in parallel: `hello` then `list_devices`, ≤300ms each.
///   - Match by exact `device.path` string. Stale sockets / dead daemons
///     just time out and get skipped.
///
/// TCP daemons are out of scope on purpose — they require an explicit
/// `-s tcp://...`, the user / agent isn't going to surprise themselves
/// with one.
async fn find_daemon_managing_device(target: &str) -> Option<ExistingDaemon> {
    use futures::future::join_all;

    let entries = match std::fs::read_dir("/tmp") {
        Ok(e) => e,
        Err(_) => return None,
    };
    let candidates: Vec<std::path::PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            let name = p
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("");
            name.starts_with("tetherd") && name.ends_with(".sock")
        })
        .collect();

    if candidates.is_empty() {
        return None;
    }

    let probes = candidates.into_iter().map(|sock_path| {
        let target = target.to_string();
        async move {
            tokio::time::timeout(
                std::time::Duration::from_millis(300),
                probe_daemon_devices(sock_path.clone(), &target),
            )
            .await
            .ok()
            .flatten()
        }
    });

    // First daemon whose device path matches wins.
    join_all(probes).await.into_iter().flatten().next()
}

/// Open one UDS socket, send `hello` + `list_devices`, return a hit if
/// any device's path matches `target`.
async fn probe_daemon_devices(
    sock_path: std::path::PathBuf,
    target: &str,
) -> Option<ExistingDaemon> {
    let stream = UnixStream::connect(&sock_path).await.ok()?;
    let mut framed = Framed::new(stream, NdjsonCodec::new());

    // hello (auth_token=None — UDS is OS-authenticated)
    let hello = Request::new(
        RpcId::Number(1),
        "hello",
        serde_json::to_value(HelloParams {
            protocol_version: tether_protocol::PROTOCOL_VERSION.to_string(),
            client: tether_protocol::ClientInfo {
                name: "tether-probe".into(),
                version: env!("CARGO_PKG_VERSION").into(),
                kind: "agent".into(),
            },
            auth_token: None,
        })
        .ok()?,
    );
    framed.send(Message::Request(hello)).await.ok()?;
    // Drain hello response (we don't care about its content).
    let _ = framed.next().await;

    // list_devices
    let list = Request::new(RpcId::Number(2), "list_devices", json!({}));
    framed.send(Message::Request(list)).await.ok()?;
    let resp = match framed.next().await? {
        Ok(Message::Response(r)) => r,
        _ => return None,
    };
    let result = match resp.payload {
        ResponsePayload::Ok { result } => result,
        _ => return None,
    };
    let devices = result.get("devices")?.as_array()?;
    for d in devices {
        let path = d.get("path").and_then(|s| s.as_str()).unwrap_or("");
        if path == target {
            let id = d.get("id").and_then(|s| s.as_str()).unwrap_or("").to_string();
            if id.is_empty() {
                return None; // shouldn't happen in v0.8+, but defensive
            }
            return Some(ExistingDaemon {
                socket: sock_path.to_string_lossy().to_string(),
                device_id: id,
            });
        }
    }
    None
}

/// RAII guard that owns the spawned daemon child. On drop, sends SIGTERM
/// (best-effort) and removes the temporary socket file.
struct DaemonGuard {
    child: Option<std::process::Child>,
    socket_path: std::path::PathBuf,
    log_path: Option<std::path::PathBuf>,
}

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            // SIGTERM first; if the daemon ignores it we'd ideally escalate,
            // but for a one-shot client exit "best effort" is fine.
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = std::fs::remove_file(&self.socket_path);
        if let Some(p) = &self.log_path {
            let _ = std::fs::remove_file(p);
        }
    }
}

/// Find the `tetherd` binary, preferring the one next to the running
/// `tether` so that (`/usr/local/bin/tether`, `/usr/local/bin/tetherd`)
/// stays in sync.
fn find_tetherd() -> Result<std::path::PathBuf, CliError> {
    // 1. Same dir as our own binary.
    if let Ok(self_exe) = std::env::current_exe() {
        if let Some(dir) = self_exe.parent() {
            let candidate = dir.join("tetherd");
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }
    // 2. Fall back to PATH.
    if let Ok(path_var) = std::env::var("PATH") {
        for dir in path_var.split(':') {
            let candidate = std::path::Path::new(dir).join("tetherd");
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }
    Err(CliError::Connection(
        "tether: -D needs the `tetherd` binary; install it with the same package as tether"
            .into(),
    ))
}

async fn spawn_embedded_daemon(
    device: &str,
    baud: u32,
    cli: &mut Cli,
) -> Result<DaemonGuard, CliError> {
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    let tetherd = find_tetherd()?;
    let pid = std::process::id();
    let nonce: u32 = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let sock_path = std::path::PathBuf::from(format!("/tmp/tether-{pid}-{nonce}.sock"));
    let log_path = std::path::PathBuf::from(format!("/tmp/tether-{pid}-{nonce}.log"));

    let log_file = std::fs::File::create(&log_path).map_err(|e| {
        CliError::Connection(format!("tether: cannot create daemon log {log_path:?}: {e}"))
    })?;
    let log_for_err = log_file.try_clone().map_err(|e| {
        CliError::Connection(format!("tether: dup log fd: {e}"))
    })?;

    // If the operator requested `--tcp`, also expose the embedded daemon
    // over the network so a remote client (e.g. an AI agent in a VM) can
    // attach for the lifetime of this standalone session. We materialise
    // the auth token here so both the spawned daemon and this very client
    // process can find each other, and so we can print it to stderr before
    // dropping into the shell.
    let mut cmd = Command::new(&tetherd);
    cmd.arg("-D").arg(device)
        .arg("-b").arg(baud.to_string())
        .arg("-s").arg(&sock_path);

    let mut tcp_banner: Option<(String, String)> = None; // (bind, token)
    if let Some(tcp_bind) = cli.tcp.clone() {
        let token = cli
            .auth_token
            .clone()
            .unwrap_or_else(random_token_hex);
        cmd.arg("--tcp").arg(&tcp_bind)
            .arg("--auth-token").arg(&token);
        cli.auth_token = Some(token.clone());
        tcp_banner = Some((tcp_bind, token));
    }

    let child = cmd
        .stdout(Stdio::from(log_file))
        .stderr(Stdio::from(log_for_err))
        .spawn()
        .map_err(|e| {
            let _ = std::fs::remove_file(&log_path);
            CliError::Connection(format!(
                "tether: failed to spawn {tetherd:?}: {e}"
            ))
        })?;

    let mut guard = DaemonGuard {
        child: Some(child),
        socket_path: sock_path.clone(),
        log_path: Some(log_path.clone()),
    };

    // Wait up to 5s for the daemon to bind its socket. If the child dies
    // before then, surface the captured banner/log so the user can see why.
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if sock_path.exists() {
            // Override the socket the rest of the client logic will dial.
            cli.socket = Some(sock_path.to_string_lossy().into_owned());
            // Surface the TCP listener info to the user *before* the shell
            // takes over the terminal. Remote attachers need the token and
            // an endpoint they can reach.
            if let Some((bind, token)) = &tcp_banner {
                eprintln!();
                eprintln!("tether: also listening on tcp://{bind}");
                eprintln!("tether:   auth token: {token}");
                eprintln!("tether:   remote clients:");
                eprintln!("tether:     TETHER_AUTH_TOKEN={token} \\");
                eprintln!("tether:       tether -s tcp://<this-host>:{} status", port_of(bind));
                eprintln!("tether:   (this daemon shuts down when you quit — Ctrl-A Q)");
                eprintln!();
            }
            return Ok(guard);
        }
        // Did the child exit early?
        if let Some(child) = guard.child.as_mut() {
            if let Ok(Some(status)) = child.try_wait() {
                let log = std::fs::read_to_string(&log_path).unwrap_or_default();
                guard.child = None; // already reaped
                return Err(CliError::Connection(format!(
                    "tether: embedded tetherd exited (status {status}) before binding socket\
                     {}\n\n{log}",
                    dash_d_hint(device)
                )));
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Err(CliError::Connection(format!(
        "tether: embedded tetherd did not bind {sock_path:?} within 5s"
    )))
}

/// When `-D <VALUE>` fails to launch an embedded daemon and `VALUE` doesn't
/// look like a device path (no `/`, and nothing by that name on disk), the
/// most likely mistake is uppercase `-D` (standalone PATH) where lowercase
/// `-d` (select a device-id on an existing daemon) was meant. Return a hint
/// to splice into the error, or an empty string when `VALUE` really is a path.
fn dash_d_hint(value: &str) -> String {
    let looks_like_path = value.contains('/') || std::path::Path::new(value).exists();
    if looks_like_path {
        String::new()
    } else {
        format!(
            "\n\nhint: `-D {value}` starts a *new* standalone daemon and opens `{value}` as a \
             device path — which doesn't exist. To select a device named `{value}` on an \
             already-running daemon, use lowercase `-d {value}` instead."
        )
    }
}

/// 32-hex random token, matches the format `tetherd` itself uses when it
/// auto-generates a token. Pulled into a helper so the spawned daemon and
/// this client agree without round-tripping through the banner log.
fn random_token_hex() -> String {
    use uuid::Uuid;
    Uuid::new_v4().simple().to_string()
}


/// Extract the `PORT` from `HOST:PORT`; falls back to the whole string if
/// no `:` (shouldn't happen with our default-missing-value, but defensive).
fn port_of(bind: &str) -> &str {
    bind.rsplit_once(':').map(|(_, p)| p).unwrap_or(bind)
}

/// Build a friendly multi-line error when the local Unix socket can't be
/// dialed — the most common cause is "the daemon hasn't been started yet".
fn make_uds_connect_error(path: &str, e: std::io::Error) -> CliError {
    use std::io::ErrorKind;
    let cause = match e.kind() {
        ErrorKind::NotFound => "socket file does not exist (daemon not running?)",
        ErrorKind::ConnectionRefused => "connection refused (stale socket from a crashed daemon?)",
        _ => return CliError::Connection(format!("connect {path}: {e}")),
    };
    let msg = format!(
        "tether: cannot connect to {path} — {cause}\n\
         \n\
         For one-off use, just give the device path — no daemon needed:\n\
         \n\
         \x20\x20tether /dev/tty.usbserial-XXXX\n\
         \n\
         For long-lived multi-client use, start a daemon first:\n\
         \n\
         \x20\x20tetherd -D /dev/tty.usbserial-XXXX -b 115200\n\
         \n\
         Or expose it for remote agents:\n\
         \n\
         \x20\x20tetherd -D /dev/tty.usbserial-XXXX -b 115200 --tcp\n\
         \n\
         (Run `tetherd --help` for all options.)"
    );
    CliError::Connection(msg)
}

fn make_tcp_connect_error(addr: &str, e: std::io::Error) -> CliError {
    let msg = format!(
        "tether: cannot connect to tcp://{addr} — {e}\n\
         \n\
         Check that the daemon is running on the remote host with:\n\
         \n\
         \x20\x20tetherd -D /dev/tty.usbserial-XXXX -b 115200 --tcp\n\
         \n\
         If a firewall is in the way, ensure the port is open.\n\
         For TCP, set TETHER_AUTH_TOKEN or pass --auth-token."
    );
    CliError::Connection(msg)
}

async fn run_with_stream<S>(mut framed: Framed<S, NdjsonCodec>, mut cli: Cli) -> Result<(), CliError>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let mut next_id: i64 = 1;

    call(
        &mut framed,
        &mut next_id,
        "hello",
        serde_json::to_value(HelloParams {
            protocol_version: tether_protocol::PROTOCOL_VERSION.to_string(),
            client: tether_protocol::ClientInfo {
                name: "tether".into(),
                version: env!("CARGO_PKG_VERSION").into(),
                kind: "agent".into(),
            },
            auth_token: cli.auth_token.clone(),
        })
        .unwrap(),
    )
    .await?;

    let cmd = cli.cmd.clone().unwrap_or(Cmd::Shell { from: "now".into() });

    // Convenience: a multi-device daemon with no `-d` selected. In an
    // interactive terminal, let the user pick which device to act on; otherwise
    // leave `device_id` unset so the per-command `AmbiguousDevice` error fires
    // (preserving scripted behavior). Skipped for commands that don't target a
    // device (status / list-devices / ports).
    if cli.device_id.is_none() && command_needs_device(&cmd) {
        let interactive = interactive_allowed(cli.no_interactive);
        if let Some(id) = maybe_pick_device(&mut framed, &mut next_id, interactive).await? {
            eprintln!("tether: using device `{id}`.");
            cli.device_id = Some(id);
        }
    }

    match cmd {
        Cmd::Status => {
            let v = call(&mut framed, &mut next_id, "status", json!({})).await?;
            print_json_or_pairs(&v, cli.json);
        }
        Cmd::Send { data, base64: is_b64, newline } => {
            let session_id = attach(&mut framed, &mut next_id, "now", cli.device_id.as_deref(), "rw", "tether").await?;
            let mut p = SendParams {
                session_id: session_id.clone(),
                data: None,
                data_text: None,
                eat_echo: false,
            };
            if is_b64 {
                p.data = Some(data);
            } else {
                p.data_text = Some(apply_newline(data, &newline));
            }
            let v = call_or_retry(
                &mut framed,
                &mut next_id,
                "send",
                serde_json::to_value(p).unwrap(),
                cli.auto_reconnect,
            )
            .await?;
            print_json_or_pairs(&v, cli.json);
        }
        Cmd::Expect {
            pattern,
            timeout_ms,
            literal,
            strip_ansi,
            max_output_bytes,
        } => {
            let session_id = attach(&mut framed, &mut next_id, "now", cli.device_id.as_deref(), "rw", "tether").await?;
            let p = ExpectParams {
                session_id,
                pattern: pattern.clone(),
                regex: !literal,
                timeout_ms: Some(timeout_ms),
                strip_ansi,
                strip_echo: None,
                from_seq: None,
                max_bytes: None,
                max_output_bytes: Some(max_output_bytes),
            };
            match call_or_retry(
                &mut framed,
                &mut next_id,
                "expect",
                serde_json::to_value(p).unwrap(),
                cli.auto_reconnect,
            )
            .await
            {
                Ok(v) => print_match_result(&v, cli.json),
                Err(e @ CliError::Timeout(_)) => {
                    eprintln!(
                        "tether: timeout after {timeout_ms}ms waiting for {pattern:?}"
                    );
                    return Err(e);
                }
                Err(e) => return Err(e),
            }
        }
        Cmd::Run {
            data,
            until,
            timeout_ms,
            literal,
            strip_ansi,
            strip_echo,
            max_output_bytes,
            preempt,
            newline,
        } => {
            // Fill in `-u` and `--newline` from the device's console
            // personality when the caller omitted them.
            let personality =
                fetch_personality(&mut framed, &mut next_id, cli.device_id.as_deref()).await;
            let until = match until.or_else(|| personality.prompt.clone()) {
                Some(u) => u,
                None => {
                    let dflag = dflag_fragment(cli.device_id.as_deref());
                    return Err(CliError::Connection(format!(
                        "tether: run needs a --until/-u pattern (none given and the device has \
                         no `prompt=` configured).\n  \
                         e.g. tether{dflag} run \"{data}\" -u \"<prompt-regex>\"\n  \
                         or set `prompt=` on the device (see -D / daemon config)."
                    )));
                }
            };
            // Line terminator: explicit flag, else the device default, else
            // `none` (run's historical default).
            let newline = newline
                .or_else(|| personality.newline.clone())
                .unwrap_or_else(|| "none".to_string());
            let session_id = attach(&mut framed, &mut next_id, "now", cli.device_id.as_deref(), "rw", "tether").await?;
            let p = RunParams {
                session_id,
                data: None,
                data_text: Some(apply_newline(data, &newline)),
                until: UntilSpec {
                    pattern: until.clone(),
                    regex: !literal,
                    strip_ansi,
                },
                timeout_ms: Some(timeout_ms),
                preempt,
                strip_echo,
                max_bytes: None,
                max_output_bytes: Some(max_output_bytes),
            };
            match call_or_retry(
                &mut framed,
                &mut next_id,
                "run",
                serde_json::to_value(p).unwrap(),
                cli.auto_reconnect,
            )
            .await
            {
                Ok(v) => print_match_result(&v, cli.json),
                Err(e @ CliError::Timeout(_)) => {
                    eprintln!(
                        "tether: timeout after {timeout_ms}ms waiting for {until:?}"
                    );
                    return Err(e);
                }
                Err(e) => return Err(e),
            }
        }
        Cmd::Exec {
            data,
            timeout_ms,
            strip_ansi,
            max_output_bytes,
            preempt,
            newline,
        } => {
            let personality =
                fetch_personality(&mut framed, &mut next_id, cli.device_id.as_deref()).await;
            let dflag = dflag_fragment(cli.device_id.as_deref());
            // shell=none is a raw/non-shell console: `exec` can't work there.
            // Fail fast with the run/send/expect recipe instead of hanging
            // until the timeout on an end marker that will never appear.
            if personality.shell == "none" {
                return Err(CliError::Connection(format!(
                    "tether: device is configured `shell=none` (raw/non-shell console) — \
                     `exec` needs a shell.\n  \
                     Use: tether{dflag} run \"{data}\" -u \"<prompt-regex>\" --newline cr\n  \
                     or:  tether{dflag} send \"...\" --newline cr  then  \
                     tether{dflag} expect \"<pattern>\""
                )));
            }
            // Line terminator. U-Boot must use CR only: its CLI runs a command
            // per CR and repeats the previous command on a bare LF, so CRLF
            // double-executes. Force `cr` there regardless of the flag (warn if
            // the caller asked for something else). Otherwise honor the flag,
            // then the device default, then `cr`.
            let newline = if personality.shell == "uboot" {
                if let Some(n) = &newline {
                    if n != "cr" {
                        eprintln!(
                            "tether: shell=uboot forces --newline cr (ignoring --newline {n}; \
                             crlf/lf double-execute U-Boot's CLI)"
                        );
                    }
                }
                "cr".to_string()
            } else {
                newline
                    .or_else(|| personality.newline.clone())
                    .unwrap_or_else(|| "cr".to_string())
            };
            let session_id = attach(&mut framed, &mut next_id, "now", cli.device_id.as_deref(), "rw", "tether").await?;
            let (wrapped, until, begin_marker) = wrap_exec_command(&data);
            let p = RunParams {
                session_id,
                data: None,
                data_text: Some(apply_newline(wrapped, &newline)),
                until: UntilSpec {
                    pattern: until,
                    regex: true,
                    strip_ansi,
                },
                timeout_ms: Some(timeout_ms),
                preempt,
                // We bracket the output with begin/end markers and slice
                // between them client-side, so the daemon's first-line echo
                // stripping (which breaks on wrapped command lines) is off.
                strip_echo: false,
                max_bytes: None,
                max_output_bytes: Some(max_output_bytes),
            };
            match call_or_retry(
                &mut framed,
                &mut next_id,
                "run",
                serde_json::to_value(p).unwrap(),
                cli.auto_reconnect,
            )
            .await
            {
                Ok(v) => {
                    // Mirror the device command's status, ssh-style: Some(0) ok,
                    // Some(n) → that code, None → unknown (non-POSIX status).
                    return match print_exec_result(&v, &begin_marker, cli.json) {
                        Some(0) => Ok(()),
                        Some(code) => Err(CliError::RemoteExit(code)),
                        None => Err(CliError::UnknownExecStatus),
                    };
                }
                Err(CliError::Timeout(buffered)) => {
                    exec_timeout_hint(
                        timeout_ms,
                        buffered.as_deref(),
                        &begin_marker,
                        &personality,
                        &dflag,
                        &data,
                    );
                    return Err(CliError::Timeout(buffered));
                }
                Err(e) => return Err(e),
            }
        }
        Cmd::Tail { from } => {
            let session_id = attach(&mut framed, &mut next_id, &from, cli.device_id.as_deref(), "rw", "tether").await?;
            tail_loop(&mut framed, &session_id).await?;
        }
        Cmd::Shell { from } => {
            let session_id = attach(&mut framed, &mut next_id, &from, cli.device_id.as_deref(), "rw", "tether").await?;
            shell_loop(framed, session_id).await?;
            return Ok(());
        }
        Cmd::Ports => {
            let v = call(&mut framed, &mut next_id, "list_ports", json!({})).await?;
            print_ports(&v, cli.json);
        }
        // Handled in `run()` before any connection; arm kept for exhaustiveness.
        Cmd::Agents => print!("{AGENTS_GUIDE}"),
        Cmd::Config {
            baud,
            data_bits,
            parity,
            stop_bits,
            flow_control,
        } => {
            let any = baud.is_some()
                || data_bits.is_some()
                || parity.is_some()
                || stop_bits.is_some()
                || flow_control.is_some();
            if !any {
                // Read-only: pull current device info from `status`. Pick the
                // targeted device out of the `devices` array (falling back to
                // the daemon's default) so `-d <id>` shows the right port.
                let v = call(&mut framed, &mut next_id, "status", json!({})).await?;
                let want = cli.device_id.clone().or_else(|| {
                    v.get("default_device").and_then(|s| s.as_str()).map(String::from)
                });
                let device = v
                    .get("devices")
                    .and_then(|a| a.as_array())
                    .and_then(|arr| {
                        arr.iter()
                            .find(|d| d.get("id").and_then(|s| s.as_str()) == want.as_deref())
                    })
                    .and_then(|d| d.get("device").cloned())
                    .or_else(|| v.get("device").cloned())
                    .unwrap_or(json!({}));
                print_device_config(&device, cli.json);
            } else {
                let mut p = serde_json::Map::new();
                if let Some(d) = &cli.device_id { p.insert("device_id".into(), json!(d)); }
                if let Some(v) = baud { p.insert("baud".into(), json!(v)); }
                if let Some(v) = data_bits { p.insert("data_bits".into(), json!(v)); }
                if let Some(v) = parity { p.insert("parity".into(), json!(v)); }
                if let Some(v) = stop_bits { p.insert("stop_bits".into(), json!(v)); }
                if let Some(v) = flow_control { p.insert("flow_control".into(), json!(v)); }
                let v = call(&mut framed, &mut next_id, "set_device", Value::Object(p)).await?;
                let device = v.get("device").cloned().unwrap_or(json!({}));
                print_device_config(&device, cli.json);
            }
        }
        Cmd::Reconnect { nowait, timeout_ms } => {
            let mut p = serde_json::Map::new();
            if let Some(d) = &cli.device_id { p.insert("device_id".into(), json!(d)); }
            p.insert("wait".into(), json!(!nowait));
            p.insert("timeout_ms".into(), json!(timeout_ms));
            let v = call(&mut framed, &mut next_id, "reconnect", Value::Object(p)).await?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
            } else {
                let reconnected = v.get("reconnected").and_then(|b| b.as_bool()).unwrap_or(false);
                let connected = v.get("device_connected").and_then(|b| b.as_bool()).unwrap_or(false);
                if connected {
                    eprintln!("tether: device reopened");
                } else if reconnected {
                    eprintln!("tether: reconnect succeeded but device state is uncertain");
                } else {
                    let reason = v.get("reason").and_then(|s| s.as_str());
                    match reason {
                        Some(r) => eprintln!(
                            "tether: reconnect requested but device is still disconnected after {timeout_ms}ms ({r})"
                        ),
                        None => eprintln!(
                            "tether: reconnect requested but device is still disconnected after {timeout_ms}ms"
                        ),
                    }
                    return Err(CliError::DeviceDisconnected);
                }
            }
        }
        Cmd::ListDevices => {
            let v = call(&mut framed, &mut next_id, "list_devices", json!({})).await?;
            print_device_list(&v, cli.json);
        }
        Cmd::Break { duration_ms } => {
            let mut p = serde_json::Map::new();
            if let Some(d) = &cli.device_id { p.insert("device_id".into(), json!(d)); }
            p.insert("duration_ms".into(), json!(duration_ms));
            let v = call(&mut framed, &mut next_id, "send_break", Value::Object(p)).await?;
            print_json_or_pairs(&v, cli.json);
        }
        Cmd::Dtr { state } => {
            let on = state == "on";
            let mut p = serde_json::Map::new();
            if let Some(d) = &cli.device_id { p.insert("device_id".into(), json!(d)); }
            p.insert("on".into(), json!(on));
            let v = call(&mut framed, &mut next_id, "set_dtr", Value::Object(p)).await?;
            print_json_or_pairs(&v, cli.json);
        }
        Cmd::Rts { state } => {
            let on = state == "on";
            let mut p = serde_json::Map::new();
            if let Some(d) = &cli.device_id { p.insert("device_id".into(), json!(d)); }
            p.insert("on".into(), json!(on));
            let v = call(&mut framed, &mut next_id, "set_rts", Value::Object(p)).await?;
            print_json_or_pairs(&v, cli.json);
        }
        Cmd::Lines => {
            let mut p = serde_json::Map::new();
            if let Some(d) = &cli.device_id { p.insert("device_id".into(), json!(d)); }
            let v = call(&mut framed, &mut next_id, "read_modem_status", Value::Object(p)).await?;
            print_modem_status(&v, cli.json);
        }
        Cmd::Reset { esp32, seq } => {
            let spec = match (esp32, seq) {
                (true, _) => "dtr=0 rts=1 wait=100 dtr=1 rts=0 wait=50 dtr=0".to_string(),
                (false, Some(s)) => s,
                (false, None) => {
                    return Err(CliError::Connection(
                        "tether reset: give a sequence — a preset (--esp32) or \
                         --seq \"dtr=0 rts=1 wait=100 dtr=1 rts=0 wait=50 dtr=0\".\n\
                         (Line state 1=asserted, 0=deasserted; wait=<ms>.)"
                            .into(),
                    ));
                }
            };
            run_reset_sequence(&mut framed, &mut next_id, cli.device_id.as_deref(), &spec).await?;
            eprintln!("tether: reset sequence complete");
        }
        Cmd::Pty { link, read_only, lock, exec } => {
            #[cfg(unix)]
            return pty_session(
                framed,
                next_id,
                PtyOptions {
                    device_id: cli.device_id.clone(),
                    link_arg: link,
                    read_only,
                    lock,
                    exec,
                    force_json: cli.json,
                },
            )
            .await;
            #[cfg(not(unix))]
            {
                let _ = (framed, next_id, link, read_only, lock, exec, cli.json);
                return Err(CliError::Connection(
                    "tether pty requires a Unix host".into(),
                ));
            }
        }
        Cmd::Disconnect => {
            let mut p = serde_json::Map::new();
            if let Some(d) = &cli.device_id { p.insert("device_id".into(), json!(d)); }
            let v = call(&mut framed, &mut next_id, "disconnect_device", Value::Object(p)).await?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
            } else {
                eprintln!("tether: device disconnected (auto-reconnect paused)");
            }
        }
        Cmd::Connect => {
            let mut p = serde_json::Map::new();
            if let Some(d) = &cli.device_id { p.insert("device_id".into(), json!(d)); }
            let v = call(&mut framed, &mut next_id, "connect_device", Value::Object(p)).await?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
            } else {
                let connected = v.get("connected").and_then(|b| b.as_bool()).unwrap_or(false);
                if connected {
                    eprintln!("tether: device reopened");
                } else {
                    eprintln!("tether: connect requested but device is still closed");
                }
            }
        }
        Cmd::Sync { idle_ms, timeout_ms } => {
            // Nudge the console with its configured line terminator (CR by
            // default, which suits U-Boot and most serial shells).
            let personality =
                fetch_personality(&mut framed, &mut next_id, cli.device_id.as_deref()).await;
            let nudge = apply_newline(String::new(), personality.newline.as_deref().unwrap_or("cr"));
            let session_id = attach(&mut framed, &mut next_id, "now", cli.device_id.as_deref(), "rw", "tether").await?;
            call(
                &mut framed,
                &mut next_id,
                "send",
                serde_json::to_value(SendParams {
                    session_id: session_id.clone(),
                    data: None,
                    data_text: Some(nudge),
                    eat_echo: false,
                })
                .unwrap(),
            )
            .await?;
            // Wait for `idle_ms` of silence (capped by `timeout_ms`).
            let prompt = sync_until_idle(&mut framed, &mut next_id, &session_id, idle_ms, timeout_ms)
                .await?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&json!({
                    "prompt_candidate": prompt,
                })).unwrap());
            } else {
                println!("{}", prompt);
            }
        }
    }
    Ok(())
}

/// Console personality of a device as reported by the daemon (`-D shell=`,
/// `prompt=`, `newline=`). Drives how `exec` frames commands / picks a line
/// terminator and what `run` uses as its default `-u`.
#[derive(Debug, Clone)]
struct Personality {
    /// "posix" | "uboot" | "none".
    shell: String,
    /// Default `-u` prompt regex for `run`, if configured.
    prompt: Option<String>,
    /// Default line terminator ("lf" | "cr" | "crlf" | "none"), if configured.
    newline: Option<String>,
}

impl Personality {
    fn posix() -> Self {
        Self { shell: "posix".into(), prompt: None, newline: None }
    }
}

/// Best-effort lookup of a device's console personality via `list_devices`.
/// Any failure (pre-personality daemon, transport hiccup, device not found)
/// falls back to the POSIX default so behaviour is unchanged in those cases.
async fn fetch_personality<S>(
    framed: &mut Framed<S, NdjsonCodec>,
    next_id: &mut i64,
    device_id: Option<&str>,
) -> Personality
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let v = match call(framed, next_id, "list_devices", json!({})).await {
        Ok(v) => v,
        Err(_) => return Personality::posix(),
    };
    let devices = v
        .get("devices")
        .and_then(|d| d.as_array())
        .cloned()
        .unwrap_or_default();
    let default_id = v.get("default_device").and_then(|s| s.as_str());
    // Mirror the daemon's device resolution: explicit id, else the only
    // device, else the daemon's default. If we can't pin one down, POSIX.
    let entry = match device_id {
        Some(want) => devices
            .iter()
            .find(|d| d.get("id").and_then(|s| s.as_str()) == Some(want)),
        None if devices.len() == 1 => devices.first(),
        None => devices
            .iter()
            .find(|d| d.get("id").and_then(|s| s.as_str()) == default_id),
    };
    match entry {
        Some(d) => Personality {
            shell: d
                .get("shell")
                .and_then(|s| s.as_str())
                .unwrap_or("posix")
                .to_string(),
            prompt: d.get("prompt").and_then(|s| s.as_str()).map(String::from),
            newline: d.get("newline").and_then(|s| s.as_str()).map(String::from),
        },
        None => Personality::posix(),
    }
}

/// Format the `-d <id>` fragment for hint messages: ` -d <id>` when a device
/// is targeted, empty otherwise. Keeps copy-pasteable hints correct in both
/// single- and multi-device daemons.
fn dflag_fragment(device_id: Option<&str>) -> String {
    match device_id {
        Some(id) => format!(" -d {id}"),
        None => String::new(),
    }
}

/// Attach a session against `device_id` (daemon default if `None`), with the
/// given replay policy, session `mode` (`"rw"` | `"ro"`), and a client
/// `label` shown in the daemon's `status`/`list-devices` session listings.
async fn attach<S>(
    framed: &mut Framed<S, NdjsonCodec>,
    next_id: &mut i64,
    replay: &str,
    device_id: Option<&str>,
    mode: &str,
    label: &str,
) -> Result<String, CliError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let p = AttachParams {
        session_id: None,
        mode: mode.into(),
        replay: tether_protocol::message::ReplaySpec::Named(replay.into()),
        label: Some(label.into()),
        flow_control: "drop_oldest".into(),
        device_id: device_id.map(|s| s.to_string()),
    };
    let v = call(framed, next_id, "attach", serde_json::to_value(p).unwrap()).await?;
    let id = v
        .get("session_id")
        .and_then(|s| s.as_str())
        .ok_or_else(|| CliError::Protocol("attach missing session_id".into()))?;
    Ok(id.to_string())
}

async fn call<S>(
    framed: &mut Framed<S, NdjsonCodec>,
    next_id: &mut i64,
    method: &str,
    params: Value,
) -> Result<Value, CliError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let id = *next_id;
    *next_id += 1;
    let req = Request::new(RpcId::Number(id), method, params);
    framed
        .send(Message::Request(req))
        .await
        .map_err(|e| CliError::Connection(e.to_string()))?;
    loop {
        let msg = match tokio::time::timeout(Duration::from_secs(120), framed.next()).await {
            Ok(Some(Ok(m))) => m,
            Ok(Some(Err(e))) => return Err(CliError::Protocol(format!("codec: {e}"))),
            Ok(None) => return Err(CliError::Connection("daemon closed connection".into())),
            Err(_) => return Err(CliError::Connection(format!("rpc timeout for {method}"))),
        };
        if let Message::Response(Response { id: rid, payload, .. }) = msg {
            if rid != RpcId::Number(id) {
                continue;
            }
            return match payload {
                ResponsePayload::Ok { result } => Ok(result),
                ResponsePayload::Err { error } => {
                    Err(map_rpc_error(error.code, error.message, error.data))
                }
            };
        }
        // Ignore notifications.
    }
}

/// Same as `call`, but if the daemon replies `device_disconnected` and
/// `auto_reconnect` is true, transparently kicks `reconnect` and retries
/// the original RPC once. The `reconnect` method itself is excluded so
/// we don't recurse.
async fn call_or_retry<S>(
    framed: &mut Framed<S, NdjsonCodec>,
    next_id: &mut i64,
    method: &str,
    params: Value,
    auto_reconnect: bool,
) -> Result<Value, CliError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let first = call(framed, next_id, method, params.clone()).await;
    if !auto_reconnect || method == "reconnect" {
        return first;
    }
    if !matches!(&first, Err(CliError::DeviceDisconnected)) {
        return first;
    }
    eprintln!("tether: device disconnected — attempting reconnect …");
    let recon = call(
        framed,
        next_id,
        "reconnect",
        json!({"wait": true, "timeout_ms": 10000}),
    )
    .await;
    match recon {
        Ok(v) if v.get("device_connected").and_then(|b| b.as_bool()).unwrap_or(false) => {
            eprintln!("tether: reconnected — retrying {method}");
            call(framed, next_id, method, params).await
        }
        Ok(_) => {
            eprintln!("tether: reconnect did not bring device back; failing");
            Err(CliError::DeviceDisconnected)
        }
        Err(e) => {
            eprintln!("tether: reconnect call failed: {e:?}");
            first
        }
    }
}

/// One step of a reset sequence.
#[derive(Debug, Clone, PartialEq)]
enum ResetStep {
    /// Drive DTR (`true` = asserted).
    Dtr(bool),
    /// Drive RTS (`true` = asserted).
    Rts(bool),
    /// Sleep for N milliseconds.
    Wait(u64),
}

/// Parse a reset spec into steps. Space/comma/tab separated: `dtr=0|1`,
/// `rts=0|1` (1 = asserted), `wait=<ms>`. Pure (no I/O) so it's unit-testable.
fn parse_reset_steps(spec: &str) -> Result<Vec<ResetStep>, String> {
    let mut steps = Vec::new();
    for step in spec.split([' ', ',', '\t', '\n']).filter(|s| !s.is_empty()) {
        let (k, v) = step
            .split_once('=')
            .ok_or_else(|| format!("bad step {step:?} (want dtr=/rts=/wait=)"))?;
        match k.trim() {
            "wait" | "delay" | "sleep" | "ms" => {
                let ms: u64 = v
                    .trim()
                    .parse()
                    .map_err(|_| format!("bad wait value {v:?} in {step:?}"))?;
                steps.push(ResetStep::Wait(ms));
            }
            line @ ("dtr" | "rts") => {
                let on = match v.trim().to_ascii_lowercase().as_str() {
                    "1" | "on" | "high" | "true" | "assert" | "asserted" => true,
                    "0" | "off" | "low" | "false" | "deassert" | "deasserted" => false,
                    _ => return Err(format!("bad line state {v:?} in {step:?} (want 0 or 1)")),
                };
                steps.push(if line == "dtr" {
                    ResetStep::Dtr(on)
                } else {
                    ResetStep::Rts(on)
                });
            }
            _ => return Err(format!("unknown step {step:?} (want dtr=/rts=/wait=)")),
        }
    }
    if steps.is_empty() {
        return Err("empty reset sequence".into());
    }
    Ok(steps)
}

/// Execute a reset sequence on the device's real port via the existing
/// `set_dtr` / `set_rts` control RPCs — works even while a virtual `pty` is
/// bridging data (line control and data are independent).
async fn run_reset_sequence<S>(
    framed: &mut Framed<S, NdjsonCodec>,
    next_id: &mut i64,
    device_id: Option<&str>,
    spec: &str,
) -> Result<(), CliError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let steps = parse_reset_steps(spec).map_err(|e| CliError::Connection(format!("tether reset: {e}")))?;
    for step in steps {
        let (method, on) = match step {
            ResetStep::Wait(ms) => {
                tokio::time::sleep(Duration::from_millis(ms)).await;
                continue;
            }
            ResetStep::Dtr(on) => ("set_dtr", on),
            ResetStep::Rts(on) => ("set_rts", on),
        };
        let mut p = serde_json::Map::new();
        if let Some(d) = device_id {
            p.insert("device_id".into(), json!(d));
        }
        p.insert("on".into(), json!(on));
        call(framed, next_id, method, Value::Object(p)).await?;
        let line = if method == "set_dtr" { "dtr" } else { "rts" };
        eprintln!(
            "tether: {line} {}",
            if on { "asserted (1)" } else { "deasserted (0)" }
        );
    }
    Ok(())
}

fn map_rpc_error(code: i32, msg: String, data: Option<Value>) -> CliError {
    use tether_protocol::error::ErrorCode as E;
    if code == E::Timeout.as_i32() {
        // Preserve the daemon's pre-match buffer (base64 in `data.buffered`)
        // so exec can classify the failure. Decode lossily; None if absent.
        let buffered = data
            .as_ref()
            .and_then(|d| d.get("buffered"))
            .and_then(|s| s.as_str())
            .and_then(|b64| base64::engine::general_purpose::STANDARD.decode(b64).ok())
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned());
        CliError::Timeout(buffered)
    } else if code == E::DeviceDisconnected.as_i32() {
        CliError::DeviceDisconnected
    } else if code == E::BufferOverflow.as_i32() {
        CliError::BufferOverflow
    } else if code == E::LockContention.as_i32() {
        CliError::LockContention
    } else if code == E::Unauthorized.as_i32() {
        CliError::Unauthorized(msg)
    } else {
        CliError::Protocol(format!("rpc {code}: {msg}"))
    }
}

async fn tail_loop<S>(
    framed: &mut Framed<S, NdjsonCodec>,
    session_id: &str,
) -> Result<(), CliError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    use std::io::Write;
    let mut stdout = std::io::stdout().lock();
    while let Some(item) = framed.next().await {
        let msg = item.map_err(|e| CliError::Protocol(e.to_string()))?;
        if let Message::Notification(Notification { method, params, .. }) = msg {
            match method.as_str() {
                "data" => {
                    if let Some(p) = params {
                        if let Some(sid) = p.get("session_id").and_then(|s| s.as_str()) {
                            if sid != session_id {
                                continue;
                            }
                        }
                        if let Some(b64) = p.get("data").and_then(|s| s.as_str()) {
                            if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(b64) {
                                stdout.write_all(&bytes).ok();
                                stdout.flush().ok();
                            }
                        }
                    }
                }
                "device" => {
                    if let Some(p) = params {
                        let kind = p.get("kind").and_then(|s| s.as_str()).unwrap_or("?");
                        let detail = p.get("detail").and_then(|s| s.as_str()).unwrap_or("");
                        let _ = stdout.flush();
                        if detail.is_empty() {
                            eprintln!("\n[device {kind}]");
                        } else {
                            eprintln!("\n[device {kind}: {detail}]");
                        }
                    }
                }
                _ => {}
            }
        }
    }
    Ok(())
}

/// Interactive raw-mode shell. Spins up:
/// - a sender task that drains an mpsc channel of outgoing JSON-RPC messages,
/// - a reader task that turns incoming `data` notifications into stdout writes,
/// - a stdin reader thread (blocking) that pushes raw bytes into a channel.
///
/// The main async loop pulls stdin chunks, scans for the Ctrl-A escape, and
/// sends keystrokes to the device as `send` RPCs. RAII guard restores the
/// terminal mode on any exit path (including panic).
async fn shell_loop<S>(framed: Framed<S, NdjsonCodec>, session_id: String) -> Result<(), CliError>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
    use std::io::{IsTerminal as _, Write as _};

    /// Restores terminal mode whenever this guard drops (Ok, Err, or panic).
    struct RawModeGuard {
        active: bool,
    }
    impl Drop for RawModeGuard {
        fn drop(&mut self) {
            if self.active {
                let _ = disable_raw_mode();
                eprint!("\r\n");
            }
        }
    }

    let is_tty = std::io::stdin().is_terminal();
    let _guard = if is_tty {
        enable_raw_mode().map_err(|e| CliError::Connection(format!("raw mode: {e}")))?;
        RawModeGuard { active: true }
    } else {
        // Piped stdin (scripts, tests) — skip raw mode but everything else works.
        RawModeGuard { active: false }
    };

    if is_tty {
        eprint!(
            "\r\n[tether shell — Ctrl-A Q quit, Ctrl-A B/D/R/L (break/dtr/rts/lines), Ctrl-A C/V (config/ports), Ctrl-A ? help]\r\n\r\n"
        );
    }

    let (mut sink, mut source) = framed.split();
    let (msg_tx, mut msg_rx) = tokio::sync::mpsc::unbounded_channel::<Message>();

    // Sender task: serialise outgoing messages to the daemon.
    let sender = tokio::spawn(async move {
        while let Some(msg) = msg_rx.recv().await {
            if sink.send(msg).await.is_err() {
                break;
            }
        }
    });

    // Side channel for RPC responses sent in reaction to in-shell escapes
    // (Ctrl-A C, Ctrl-A V). Notifications still go to stdout/stderr; responses
    // are forwarded here so the escape handler can await the result.
    let (resp_tx, mut resp_rx) = tokio::sync::mpsc::unbounded_channel::<tether_protocol::Response>();

    // Reader task: stream `data` notifications for our session straight to
    // stdout, and surface `device` (dis|re)connect events to stderr.
    let target = session_id.clone();
    let reader = tokio::spawn(async move {
        while let Some(item) = source.next().await {
            let Ok(msg) = item else { break };
            let msg = match msg {
                Message::Response(r) => {
                    let _ = resp_tx.send(r);
                    continue;
                }
                other => other,
            };
            let Message::Notification(Notification { method, params, .. }) = msg else {
                continue;
            };
            if method == "device" {
                if let Some(p) = params {
                    let kind = p.get("kind").and_then(|s| s.as_str()).unwrap_or("?");
                    let detail = p.get("detail").and_then(|s| s.as_str()).unwrap_or("");
                    if detail.is_empty() {
                        eprint!("\r\n[device {kind}]\r\n");
                    } else {
                        eprint!("\r\n[device {kind}: {detail}]\r\n");
                    }
                }
                continue;
            }
            if method != "data" {
                continue;
            }
            {
                let Some(p) = params else { continue };
                if let Some(sid) = p.get("session_id").and_then(|s| s.as_str()) {
                    if sid != target {
                        continue;
                    }
                }
                if let Some(b64) = p.get("data").and_then(|s| s.as_str()) {
                    if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(b64) {
                        let mut stdout = std::io::stdout().lock();
                        let _ = stdout.write_all(&bytes);
                        let _ = stdout.flush();
                    }
                }
            }
        }
    });

    // Stdin reader thread (blocking). Pushes chunks into an mpsc.
    let (stdin_tx, mut stdin_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    std::thread::spawn(move || {
        use std::io::Read;
        let stdin = std::io::stdin();
        let mut handle = stdin.lock();
        let mut buf = [0u8; 4096];
        loop {
            match handle.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if stdin_tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    let mut next_id: i64 = 1_000_000;
    let mut escape_pending = false;
    // Track DTR / RTS state locally so Ctrl-A D / R can toggle. Both lines
    // are conventionally asserted on a freshly-opened port.
    let mut dtr_on = true;
    let mut rts_on = true;

    // Helper closure: send a chunk of bytes via the message channel.
    let send_bytes = |bytes: &[u8], next_id: &mut i64| -> bool {
        if bytes.is_empty() {
            return true;
        }
        let p = SendParams {
            session_id: session_id.clone(),
            data: Some(base64::engine::general_purpose::STANDARD.encode(bytes)),
            data_text: None,
            eat_echo: false,
        };
        let id = *next_id;
        *next_id += 1;
        let req = Request::new(RpcId::Number(id), "send", serde_json::to_value(p).unwrap());
        msg_tx.send(Message::Request(req)).is_ok()
    };

    let print_help = || {
        eprint!(
            "\r\n[Ctrl-A Q  : quit]\r\n\
             [Ctrl-A C  : show serial config]\r\n\
             [Ctrl-A V  : list available ports]\r\n\
             [Ctrl-A B  : send BREAK]\r\n\
             [Ctrl-A D  : toggle DTR]\r\n\
             [Ctrl-A R  : toggle RTS]\r\n\
             [Ctrl-A L  : show modem status (CTS/DSR/RI/DCD)]\r\n\
             [Ctrl-A ?  : help]\r\n\
             [Ctrl-A ^A : send literal Ctrl-A]\r\n\r\n"
        );
    };

    // Pretty-print a `set_device`-style result inside the raw-mode shell.
    let print_config_inline = |device: &Value| {
        let baud = device.get("baud").and_then(|n| n.as_u64()).unwrap_or(0);
        let data_bits = device.get("data_bits").and_then(|n| n.as_u64()).unwrap_or(8);
        let parity = device.get("parity").and_then(|s| s.as_str()).unwrap_or("none");
        let stop_bits = device.get("stop_bits").and_then(|n| n.as_u64()).unwrap_or(1);
        let flow = device.get("flow_control").and_then(|s| s.as_str()).unwrap_or("none");
        let parity_letter = parity
            .chars()
            .next()
            .map(|c| c.to_ascii_uppercase())
            .unwrap_or('?');
        eprint!(
            "\r\n[config: {baud} {data_bits}{parity_letter}{stop_bits} flow={flow}]\r\n"
        );
    };

    let print_ports_inline = |ports: &[Value]| {
        if ports.is_empty() {
            eprint!("\r\n[no serial ports detected]\r\n");
            return;
        }
        eprint!("\r\n[ports]\r\n");
        for port in ports {
            let path = port.get("path").and_then(|s| s.as_str()).unwrap_or("?");
            let kind = port.get("kind").and_then(|s| s.as_str()).unwrap_or("unknown");
            let desc = port
                .get("product")
                .and_then(|s| s.as_str())
                .or_else(|| port.get("manufacturer").and_then(|s| s.as_str()))
                .unwrap_or("");
            if desc.is_empty() {
                eprint!("  {path}  ({kind})\r\n");
            } else {
                eprint!("  {path}  ({kind})  {desc}\r\n");
            }
        }
        eprint!("\r\n");
    };

    'main: while let Some(chunk) = stdin_rx.recv().await {
        let mut buffer: Vec<u8> = Vec::with_capacity(chunk.len());
        for &b in &chunk {
            if escape_pending {
                escape_pending = false;
                match b {
                    b'q' | b'Q' => {
                        if !buffer.is_empty() {
                            let _ = send_bytes(&buffer, &mut next_id);
                        }
                        break 'main;
                    }
                    0x01 => buffer.push(0x01), // literal Ctrl-A
                    b'?' | b'h' | b'H' => print_help(),
                    b'c' | b'C' => {
                        // Flush any keystrokes accumulated before the escape so
                        // they don't get mixed into the post-escape output.
                        if !buffer.is_empty() {
                            let _ = send_bytes(&buffer, &mut next_id);
                            buffer.clear();
                        }
                        let id = next_id; next_id += 1;
                        let req = Request::new(RpcId::Number(id), "status", json!({}));
                        let _ = msg_tx.send(Message::Request(req));
                        match recv_response_for(id, &mut resp_rx, std::time::Duration::from_millis(2000)).await {
                            Some(tether_protocol::message::ResponsePayload::Ok { result }) => {
                                let device = result.get("device").cloned().unwrap_or(json!({}));
                                print_config_inline(&device);
                            }
                            Some(tether_protocol::message::ResponsePayload::Err { error }) => {
                                eprint!("\r\n[config error: {}]\r\n", error.message);
                            }
                            None => eprint!("\r\n[config: timeout]\r\n"),
                        }
                    }
                    b'v' | b'V' => {
                        if !buffer.is_empty() {
                            let _ = send_bytes(&buffer, &mut next_id);
                            buffer.clear();
                        }
                        let id = next_id; next_id += 1;
                        let req = Request::new(RpcId::Number(id), "list_ports", json!({}));
                        let _ = msg_tx.send(Message::Request(req));
                        match recv_response_for(id, &mut resp_rx, std::time::Duration::from_millis(2000)).await {
                            Some(tether_protocol::message::ResponsePayload::Ok { result }) => {
                                let empty = Vec::<Value>::new();
                                let ports = result
                                    .get("ports")
                                    .and_then(|p| p.as_array())
                                    .unwrap_or(&empty);
                                print_ports_inline(ports);
                            }
                            Some(tether_protocol::message::ResponsePayload::Err { error }) => {
                                eprint!("\r\n[list_ports error: {}]\r\n", error.message);
                            }
                            None => eprint!("\r\n[list_ports: timeout]\r\n"),
                        }
                    }
                    b'b' | b'B' => {
                        if !buffer.is_empty() {
                            let _ = send_bytes(&buffer, &mut next_id);
                            buffer.clear();
                        }
                        let id = next_id; next_id += 1;
                        let req = Request::new(
                            RpcId::Number(id),
                            "send_break",
                            json!({"duration_ms": 250}),
                        );
                        let _ = msg_tx.send(Message::Request(req));
                        match recv_response_for(id, &mut resp_rx, std::time::Duration::from_millis(2000)).await {
                            Some(tether_protocol::message::ResponsePayload::Ok { .. }) => {
                                eprint!("\r\n[break: 250ms]\r\n");
                            }
                            Some(tether_protocol::message::ResponsePayload::Err { error }) => {
                                eprint!("\r\n[break error: {}]\r\n", error.message);
                            }
                            None => eprint!("\r\n[break: timeout]\r\n"),
                        }
                    }
                    b'd' | b'D' => {
                        if !buffer.is_empty() {
                            let _ = send_bytes(&buffer, &mut next_id);
                            buffer.clear();
                        }
                        dtr_on = !dtr_on;
                        let id = next_id; next_id += 1;
                        let req = Request::new(
                            RpcId::Number(id),
                            "set_dtr",
                            json!({"on": dtr_on}),
                        );
                        let _ = msg_tx.send(Message::Request(req));
                        match recv_response_for(id, &mut resp_rx, std::time::Duration::from_millis(2000)).await {
                            Some(tether_protocol::message::ResponsePayload::Ok { .. }) => {
                                eprint!(
                                    "\r\n[DTR: {}]\r\n",
                                    if dtr_on { "asserted" } else { "deasserted" }
                                );
                            }
                            Some(tether_protocol::message::ResponsePayload::Err { error }) => {
                                // Roll back local state — apply failed.
                                dtr_on = !dtr_on;
                                eprint!("\r\n[DTR error: {}]\r\n", error.message);
                            }
                            None => {
                                dtr_on = !dtr_on;
                                eprint!("\r\n[DTR: timeout]\r\n");
                            }
                        }
                    }
                    b'r' | b'R' => {
                        if !buffer.is_empty() {
                            let _ = send_bytes(&buffer, &mut next_id);
                            buffer.clear();
                        }
                        rts_on = !rts_on;
                        let id = next_id; next_id += 1;
                        let req = Request::new(
                            RpcId::Number(id),
                            "set_rts",
                            json!({"on": rts_on}),
                        );
                        let _ = msg_tx.send(Message::Request(req));
                        match recv_response_for(id, &mut resp_rx, std::time::Duration::from_millis(2000)).await {
                            Some(tether_protocol::message::ResponsePayload::Ok { .. }) => {
                                eprint!(
                                    "\r\n[RTS: {}]\r\n",
                                    if rts_on { "asserted" } else { "deasserted" }
                                );
                            }
                            Some(tether_protocol::message::ResponsePayload::Err { error }) => {
                                rts_on = !rts_on;
                                eprint!("\r\n[RTS error: {}]\r\n", error.message);
                            }
                            None => {
                                rts_on = !rts_on;
                                eprint!("\r\n[RTS: timeout]\r\n");
                            }
                        }
                    }
                    b'l' | b'L' => {
                        if !buffer.is_empty() {
                            let _ = send_bytes(&buffer, &mut next_id);
                            buffer.clear();
                        }
                        let id = next_id; next_id += 1;
                        let req = Request::new(
                            RpcId::Number(id),
                            "read_modem_status",
                            json!({}),
                        );
                        let _ = msg_tx.send(Message::Request(req));
                        match recv_response_for(id, &mut resp_rx, std::time::Duration::from_millis(2000)).await {
                            Some(tether_protocol::message::ResponsePayload::Ok { result }) => {
                                let bit = |k: &str| result.get(k).and_then(|b| b.as_bool()).unwrap_or(false);
                                let m = |b| if b { "1" } else { "0" };
                                eprint!(
                                    "\r\n[lines: CTS={} DSR={} RI={} DCD={}]\r\n",
                                    m(bit("cts")), m(bit("dsr")), m(bit("ri")), m(bit("dcd"))
                                );
                            }
                            Some(tether_protocol::message::ResponsePayload::Err { error }) => {
                                eprint!("\r\n[lines error: {}]\r\n", error.message);
                            }
                            None => eprint!("\r\n[lines: timeout]\r\n"),
                        }
                    }
                    other => {
                        eprint!("\r\n[unknown escape: Ctrl-A {}]\r\n", other as char);
                    }
                }
            } else if b == 0x01 {
                escape_pending = true;
            } else {
                buffer.push(b);
            }
        }
        if !buffer.is_empty() && !send_bytes(&buffer, &mut next_id) {
            break;
        }
    }

    // Cleanup: drop sender (flushes remaining), then await tasks.
    drop(msg_tx);
    let _ = sender.await;
    reader.abort();
    Ok(())
}

// ──────────────────────────────────────────────────────────────────────────
// `tether pty` — on-demand, client-side virtual serial port.
//
// Mirrors tetherd's daemon-side `pty=` bridge (see `tetherd/pty.rs`) but
// lives entirely in the client: `openpty()` runs here, so it works against a
// remote daemon too (`-s tcp://...`). Bytes flow over the *existing*
// protocol — `data` notifications in, pipelined `send` requests out — no
// daemon changes needed for the data path. Unix-only (openpty / termios /
// unix signals); every item below is `#[cfg(unix)]`.
// ──────────────────────────────────────────────────────────────────────────

/// Async wrapper around the pty master fd. Mirrors `tetherd`'s `PtyMaster`
/// (see `tetherd/pty.rs`); duplicated here because `tether` and `tetherd`
/// are separate binaries in this crate with no shared lib target.
#[cfg(unix)]
struct PtyMaster {
    fd: AsyncFd<OwnedFd>,
}

#[cfg(unix)]
impl PtyMaster {
    async fn read(&self, buf: &mut [u8]) -> std::io::Result<usize> {
        loop {
            let mut guard = self.fd.readable().await?;
            let raw = self.fd.get_ref().as_raw_fd();
            // SAFETY: raw fd is valid; buf is a writable slice.
            let n = unsafe { libc::read(raw, buf.as_mut_ptr() as *mut _, buf.len()) };
            if n >= 0 {
                return Ok(n as usize);
            }
            let e = std::io::Error::last_os_error();
            if e.kind() == std::io::ErrorKind::WouldBlock {
                guard.clear_ready();
                continue;
            }
            return Err(e);
        }
    }

    async fn write_all(&self, mut data: &[u8]) -> std::io::Result<()> {
        while !data.is_empty() {
            let mut guard = self.fd.writable().await?;
            let raw = self.fd.get_ref().as_raw_fd();
            // SAFETY: raw fd is valid; data is a readable slice.
            let n = unsafe { libc::write(raw, data.as_ptr() as *const _, data.len()) };
            if n >= 0 {
                data = &data[n as usize..];
                continue;
            }
            let e = std::io::Error::last_os_error();
            if e.kind() == std::io::ErrorKind::WouldBlock {
                guard.clear_ready();
                continue;
            }
            return Err(e);
        }
        Ok(())
    }
}

/// `openpty()` + raw termios + non-blocking master, matching `tetherd`'s
/// `create_pty` (see `tetherd/pty.rs`). Returns the async master, the
/// slave's `/dev/...` path, and the slave fd — the caller keeps the fd open
/// for as long as the virtual port should survive tool open/close cycles.
#[cfg(unix)]
fn create_client_pty() -> Result<(PtyMaster, String, OwnedFd), CliError> {
    let mut master: RawFd = -1;
    let mut slave: RawFd = -1;
    let mut tio: libc::termios = unsafe { std::mem::zeroed() };
    unsafe { libc::cfmakeraw(&mut tio) };
    // SAFETY: out-params are valid; termios is initialized; win size null.
    let r = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            &mut tio,
            std::ptr::null_mut(),
        )
    };
    if r != 0 {
        return Err(CliError::Connection(format!(
            "tether pty: openpty: {}",
            std::io::Error::last_os_error()
        )));
    }

    let mut name_buf = [0 as libc::c_char; 256];
    // SAFETY: slave is a valid tty fd; buffer is sized.
    let rc = unsafe { libc::ttyname_r(slave, name_buf.as_mut_ptr(), name_buf.len()) };
    if rc != 0 {
        // SAFETY: both fds were just opened by openpty above.
        unsafe {
            libc::close(master);
            libc::close(slave);
        }
        return Err(CliError::Connection(format!(
            "tether pty: ttyname_r: {}",
            std::io::Error::from_raw_os_error(rc)
        )));
    }
    // SAFETY: ttyname_r NUL-terminated the buffer on success.
    let slave_name = unsafe { std::ffi::CStr::from_ptr(name_buf.as_ptr()) }
        .to_string_lossy()
        .into_owned();

    // Non-blocking master so AsyncFd can drive it.
    // SAFETY: master is a valid fd.
    unsafe {
        let flags = libc::fcntl(master, libc::F_GETFL);
        libc::fcntl(master, libc::F_SETFL, flags | libc::O_NONBLOCK);
    }

    // SAFETY: master/slave were just produced by openpty and are owned here.
    let master_owned = unsafe { OwnedFd::from_raw_fd(master) };
    let slave_owned = unsafe { OwnedFd::from_raw_fd(slave) };
    let afd = AsyncFd::with_interest(master_owned, Interest::READABLE | Interest::WRITABLE)
        .map_err(|e| CliError::Connection(format!("tether pty: AsyncFd for pty master: {e}")))?;
    Ok((PtyMaster { fd: afd }, slave_name, slave_owned))
}

/// Default symlink path for the `n`th virtual port of a device (or the
/// literal `"port"` fallback when no `-d` is given). Slots start at 1.
#[cfg(unix)]
fn default_pty_link(id_or_port: &str, n: u32) -> String {
    format!("/tmp/tether-{id_or_port}-{n}.pty")
}

/// `kill(pid, 0)` liveness probe: `true` unless the pid is confirmed gone
/// (`ESRCH`). `EPERM` (exists, owned by someone else) and any other
/// unexpected errno are treated as "alive" — never guess a link is free.
#[cfg(unix)]
fn process_alive(pid: i32) -> bool {
    // SAFETY: signal 0 probes existence; it delivers nothing.
    if unsafe { libc::kill(pid, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

/// Pure decision behind the `.pid` sidecar staleness check: given the
/// sidecar's raw contents (`None` if missing/unreadable) and a liveness
/// probe, decide whether the recorded holder is confirmed gone. `None`
/// means "can't tell" (missing/unreadable/non-numeric) — the caller must
/// treat that the same as "still alive".
#[cfg(unix)]
fn pid_sidecar_is_stale(contents: Option<&str>, probe: impl Fn(i32) -> bool) -> Option<bool> {
    let pid: i32 = contents?.trim().parse().ok()?;
    Some(!probe(pid))
}

/// If `<link>.pid` names a process that's confirmed gone, remove the stale
/// link + sidecar and report `true` (caller should retry the claim).
/// Otherwise (alive, or we can't tell) leaves everything alone.
#[cfg(unix)]
fn reclaim_if_stale(link: &str) -> bool {
    let sidecar = format!("{link}.pid");
    let contents = std::fs::read_to_string(&sidecar).ok();
    if pid_sidecar_is_stale(contents.as_deref(), process_alive) == Some(true) {
        let _ = std::fs::remove_file(link);
        let _ = std::fs::remove_file(&sidecar);
        true
    } else {
        false
    }
}

/// Atomically claim `link` -> `slave`. A live collision reports `Ok(false)`
/// ("busy, try something else"); a stale one is reclaimed and retried
/// transparently. Never removes a link without first proving its holder is
/// dead — a stale link can point at a pty slave the OS has since recycled
/// to an unrelated process.
#[cfg(unix)]
fn try_claim_link(link: &str, slave: &str) -> Result<bool, CliError> {
    loop {
        match std::os::unix::fs::symlink(slave, link) {
            Ok(()) => return Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                if reclaim_if_stale(link) {
                    continue; // same path, now clear — retry once
                }
                return Ok(false);
            }
            Err(e) => return Err(CliError::Connection(format!("tether pty: symlink {link}: {e}"))),
        }
    }
}

/// Claim a link for the virtual port: `--link PATH` verbatim, or the
/// default `/tmp/tether-<id-or-port>-<n>.pty` sequence starting at `n=1`.
/// On success, writes our pid into `<link>.pid` so a future invocation can
/// tell a stale leftover from one that's still running.
#[cfg(unix)]
fn claim_link(explicit: Option<&str>, id_or_port: &str, slave: &str) -> Result<String, CliError> {
    let link = match explicit {
        Some(path) => {
            if !try_claim_link(path, slave)? {
                return Err(CliError::Connection(format!(
                    "tether pty: --link {path} is busy (claimed by a live process)"
                )));
            }
            path.to_string()
        }
        None => {
            let mut claimed = None;
            for n in 1..=9999u32 {
                let candidate = default_pty_link(id_or_port, n);
                if try_claim_link(&candidate, slave)? {
                    claimed = Some(candidate);
                    break;
                }
            }
            claimed.ok_or_else(|| {
                CliError::Connection(
                    "tether pty: no free /tmp/tether-*.pty slot found (9999 in use?)".into(),
                )
            })?
        }
    };
    if let Err(e) = std::fs::write(format!("{link}.pid"), std::process::id().to_string()) {
        let _ = std::fs::remove_file(&link);
        return Err(CliError::Connection(format!("tether pty: write {link}.pid: {e}")));
    }
    Ok(link)
}

/// Removes the claimed symlink + `.pid` sidecar on every exit path
/// (including panics), so a crashed or interrupted `tether pty` never
/// leaves a stale link for the next invocation to trip over.
#[cfg(unix)]
struct PtyLinkGuard {
    link: String,
}

#[cfg(unix)]
impl Drop for PtyLinkGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.link);
        let _ = std::fs::remove_file(format!("{}.pid", self.link));
    }
}

/// Session label: `pty:<user>@<host>:<link>`, falling back to `pty:<link>`
/// if the hostname lookup fails. Best-effort — shows up in the daemon's
/// session listings so `status`/`list-devices` output identifies which
/// virtual port a session belongs to.
#[cfg(unix)]
fn pty_label(link: &str) -> String {
    let user = std::env::var("USER").unwrap_or_else(|_| "?".to_string());
    match hostname_best_effort() {
        Some(host) => format!("pty:{user}@{host}:{link}"),
        None => format!("pty:{link}"),
    }
}

#[cfg(unix)]
fn hostname_best_effort() -> Option<String> {
    let mut buf = [0u8; 256];
    // SAFETY: buf is a valid, sized C buffer for gethostname to fill.
    let r = unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) };
    if r != 0 {
        return None;
    }
    // SAFETY: gethostname NUL-terminates the buffer on success.
    let cstr = unsafe { std::ffi::CStr::from_ptr(buf.as_ptr() as *const libc::c_char) };
    cstr.to_str().ok().map(String::from)
}

/// True when an RPC failed because the daemon doesn't know the method at all
/// (`-32601`) — i.e. a `tetherd` older than this client feature.
#[cfg(unix)]
fn is_method_not_found(e: &CliError) -> bool {
    use tether_protocol::error::ErrorCode as E;
    matches!(e, CliError::Protocol(msg) if msg.starts_with(&format!("rpc {}: ", E::MethodNotFound.as_i32())))
}

/// Substitute `{}` in `args` with `path`; if no arg contains `{}`, append
/// `path` as a final argument. Pure so it's unit-tested without spawning.
#[cfg(unix)]
fn substitute_link_placeholder(args: &[String], path: &str) -> Vec<String> {
    let mut found = false;
    let mut out: Vec<String> = args
        .iter()
        .map(|a| {
            if a.contains("{}") {
                found = true;
                a.replace("{}", path)
            } else {
                a.clone()
            }
        })
        .collect();
    if !found {
        out.push(path.to_string());
    }
    out
}

/// ≤1-per-window stderr rate limiter for the bridge's drop/error warnings.
#[cfg(unix)]
struct RateLimiter {
    window: Duration,
    last: Option<Instant>,
}

#[cfg(unix)]
impl RateLimiter {
    fn new(window: Duration) -> Self {
        Self { window, last: None }
    }

    /// `true` at most once per `window`; only print / reset counters when
    /// this returns `true`.
    fn ready(&mut self) -> bool {
        let now = Instant::now();
        match self.last {
            Some(t) if now.duration_since(t) < self.window => false,
            _ => {
                self.last = Some(now);
                true
            }
        }
    }
}

/// Check the pty slave's termios; if the attached tool switched it out of
/// raw mode (`ICANON`/`ECHO`, or `ICRNL`/`ONLCR` munging), reassert
/// `cfmakeraw` and report `true`. Left uncorrected, a cooked/echo slave
/// reflects device bytes back out over the bridge (an echo storm) and
/// mangles binary data.
#[cfg(unix)]
fn reassert_raw_if_needed(slave_fd: RawFd) -> bool {
    let mut tio: libc::termios = unsafe { std::mem::zeroed() };
    // SAFETY: slave_fd is a valid, open tty fd for the whole bridge's life.
    if unsafe { libc::tcgetattr(slave_fd, &mut tio) } != 0 {
        return false; // transient failure; we'll check again next tick
    }
    let cooked = tio.c_lflag & (libc::ICANON | libc::ECHO) != 0
        || tio.c_iflag & libc::ICRNL != 0
        || tio.c_oflag & libc::ONLCR != 0;
    if !cooked {
        return false;
    }
    unsafe { libc::cfmakeraw(&mut tio) };
    // SAFETY: same fd; termios now holds the raw settings we just built.
    unsafe { libc::tcsetattr(slave_fd, libc::TCSANOW, &tio) == 0 }
}

/// Spawn the `-- CMD ARG...` child once the port is ready. `{}` anywhere in
/// `exec` (including the command itself) is replaced by the link path;
/// `TETHER_PTY` is also set so scripts can pick it up without relying on
/// argv. Stdio is inherited so interactive tools (minicom, screen) work
/// normally.
#[cfg(unix)]
fn spawn_exec_child(exec: &[String], link: &str) -> Result<tokio::process::Child, CliError> {
    let substituted = substitute_link_placeholder(exec, link);
    let (prog, args) = substituted
        .split_first()
        .ok_or_else(|| CliError::Connection("tether pty: -- needs a command".into()))?;
    tokio::process::Command::new(prog)
        .args(args)
        .env("TETHER_PTY", link)
        .spawn()
        .map_err(|e| CliError::Connection(format!("tether pty: exec {prog}: {e}")))
}

/// Waits on `child` if present; never resolves when it's `None`, so it's
/// safe to poll unconditionally as one arm of `tokio::select!`.
#[cfg(unix)]
async fn wait_child(
    child: Option<&mut tokio::process::Child>,
) -> std::io::Result<std::process::ExitStatus> {
    match child {
        Some(c) => c.wait().await,
        None => std::future::pending().await,
    }
}

/// Parsed `tether pty` options, bundled into one struct so `pty_session`'s
/// signature stays under clippy's argument-count lint.
#[cfg(unix)]
struct PtyOptions {
    device_id: Option<String>,
    link_arg: Option<String>,
    read_only: bool,
    lock: bool,
    exec: Vec<String>,
    force_json: bool,
}

/// `tether pty` entry point: create the pty, claim its link, attach the
/// session (optionally taking the writer lock), announce readiness, spawn
/// `-- CMD` if given, then bridge bytes until Ctrl-C / SIGTERM / child exit /
/// transport loss. See the `Cmd::Pty` doc comment for the user-facing
/// contract this implements.
#[cfg(unix)]
async fn pty_session<S>(
    mut framed: Framed<S, NdjsonCodec>,
    mut next_id: i64,
    opts: PtyOptions,
) -> Result<(), CliError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    use std::io::Write as _;
    let PtyOptions { device_id, link_arg, read_only, lock, exec, force_json } = opts;

    // 1. Create the pty before touching the daemon at all — a failure here
    //    (no pty slots, sandboxed host, …) shouldn't leave a session
    //    attached with nothing to bridge to.
    let (master, slave_name, slave_keep) = create_client_pty()?;
    let slave_raw_fd = slave_keep.as_raw_fd();

    // 2. Claim the symlink.
    let id_or_port = device_id.as_deref().unwrap_or("port");
    let link = claim_link(link_arg.as_deref(), id_or_port, &slave_name)?;
    let _link_guard = PtyLinkGuard { link: link.clone() };

    // 3. Attach the session.
    let mode = if read_only { "ro" } else { "rw" };
    let label = pty_label(&link);
    let session_id =
        attach(&mut framed, &mut next_id, "now", device_id.as_deref(), mode, &label).await?;

    // 4. --lock.
    if lock {
        let p = LockParams {
            session_id: session_id.clone(),
            device_id: device_id.clone(),
            preempt: "queue".into(),
        };
        match call(&mut framed, &mut next_id, "lock", serde_json::to_value(p).unwrap()).await {
            Ok(v) => {
                if !v.get("locked").and_then(|b| b.as_bool()).unwrap_or(false) {
                    return Err(CliError::Connection(format!(
                        "tether pty: --lock: daemon did not confirm the lock ({v})"
                    )));
                }
                eprintln!("tether: writer lock held for this session");
            }
            Err(e) if is_method_not_found(&e) => {
                eprintln!("tether: this daemon doesn't support --lock (upgrade tetherd)");
                return Err(CliError::RemoteExit(2));
            }
            Err(e) => return Err(e),
        }
    }

    // 5. Readiness — exactly one stdout write, then flush.
    {
        let mut stdout = std::io::stdout().lock();
        if force_json {
            let line = json!({
                "event": "ready",
                "path": link,
                "device": device_id,
                "session_id": session_id,
            });
            let _ = writeln!(stdout, "{line}");
        } else if exec.is_empty() {
            let _ = writeln!(stdout, "{link}");
        }
        let _ = stdout.flush();
    }
    eprintln!("tether: virtual port ready: {link} (Ctrl-C to stop)");

    // 6. Optional `-- CMD`, spawned only after readiness is printed either
    //    way (CMD itself gets the path via `{}` / `TETHER_PTY`).
    let mut child = if exec.is_empty() {
        None
    } else {
        Some(spawn_exec_child(&exec, &link)?)
    };

    // 7. Bridge until Ctrl-C / SIGTERM / child exit / transport loss.
    run_pty_bridge(framed, session_id, master, slave_raw_fd, link, read_only, &mut child).await
}

/// The byte bridge itself, once setup (pty, link, attach, lock) is done.
#[cfg(unix)]
async fn run_pty_bridge<S>(
    framed: Framed<S, NdjsonCodec>,
    session_id: String,
    master: PtyMaster,
    slave_raw_fd: RawFd,
    link: String,
    read_only: bool,
    child: &mut Option<tokio::process::Child>,
) -> Result<(), CliError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (mut sink, mut source) = framed.split();
    let mut next_id: i64 = 1;

    let mut write_drop_limiter = RateLimiter::new(Duration::from_secs(5));
    let mut read_only_limiter = RateLimiter::new(Duration::from_secs(5));
    let mut send_err_limiter = RateLimiter::new(Duration::from_secs(5));
    let mut dropped_write_bytes: u64 = 0;
    let mut dropped_typed_bytes: u64 = 0;

    let mut termios_tick = tokio::time::interval(Duration::from_millis(500));
    termios_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .map_err(|e| CliError::Connection(format!("tether pty: sigterm handler: {e}")))?;

    let mut master_buf = [0u8; 4096];

    let outcome = loop {
        tokio::select! {
            biased;

            _ = tokio::signal::ctrl_c() => {
                eprintln!("tether: stopping ({link})");
                break Ok(());
            }
            _ = sigterm.recv() => {
                eprintln!("tether: stopping ({link})");
                break Ok(());
            }
            status = wait_child(child.as_mut()) => {
                let code = match status {
                    Ok(s) => s.code().map(|c| c as u8).unwrap_or(1), // signal death → 1
                    Err(e) => {
                        eprintln!("tether: waiting on child failed: {e}");
                        1
                    }
                };
                eprintln!("tether: child exited ({code}); stopping bridge");
                break Err(CliError::RemoteExit(code));
            }

            // tool → device: read the pty master, fire a pipelined `send`
            // (no waiting on its response — see the `source.next()` arm).
            r = master.read(&mut master_buf) => {
                match r {
                    Ok(0) => tokio::time::sleep(Duration::from_millis(50)).await,
                    Ok(n) => {
                        if read_only {
                            dropped_typed_bytes += n as u64;
                            if read_only_limiter.ready() {
                                eprintln!(
                                    "tether: read-only: dropped {dropped_typed_bytes} bytes typed into {link}"
                                );
                                dropped_typed_bytes = 0;
                            }
                        } else {
                            let id = next_id;
                            next_id += 1;
                            let p = SendParams {
                                session_id: session_id.clone(),
                                data: Some(base64::engine::general_purpose::STANDARD.encode(&master_buf[..n])),
                                data_text: None,
                                eat_echo: false,
                            };
                            let req = Request::new(RpcId::Number(id), "send", serde_json::to_value(p).unwrap());
                            if sink.send(Message::Request(req)).await.is_err() {
                                break Err(CliError::Connection("tether pty: daemon connection closed".into()));
                            }
                        }
                    }
                    Err(e) => break Err(CliError::Connection(format!("tether pty: pty master read: {e}"))),
                }
            }

            // device → tool (data notifications), plus responses to our
            // fire-and-forget `send`s (drained; only errors are surfaced).
            item = source.next() => {
                match item {
                    None => {
                        eprintln!("tether: daemon closed the connection — stopping ({link})");
                        // TODO: session resume within the daemon's 30s window.
                        break Err(CliError::Connection("daemon closed connection".into()));
                    }
                    Some(Err(e)) => break Err(CliError::Connection(format!("tether pty: codec error: {e}"))),
                    Some(Ok(Message::Response(Response { payload: ResponsePayload::Err { error }, .. }))) => {
                        if send_err_limiter.ready() {
                            eprintln!("tether: send error: {} ({})", error.message, error.code);
                        }
                    }
                    Some(Ok(Message::Response(_))) => {} // ok responses to fire-and-forget sends
                    Some(Ok(Message::Request(_))) => {}   // daemon never sends requests
                    Some(Ok(Message::Notification(Notification { method, params, .. }))) => {
                        if method == "data" {
                            let Some(p) = params else { continue };
                            let same_session = p
                                .get("session_id")
                                .and_then(|s| s.as_str())
                                .map(|s| s == session_id)
                                .unwrap_or(true);
                            if !same_session {
                                continue;
                            }
                            let Some(b64) = p.get("data").and_then(|s| s.as_str()) else { continue };
                            let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(b64) else { continue };
                            // Never park forever: the pty's kernel buffer is
                            // tiny (~1KB on macOS), so a stopped tool fills
                            // it almost immediately.
                            match tokio::time::timeout(Duration::from_millis(200), master.write_all(&bytes)).await {
                                Ok(Ok(())) => {}
                                Ok(Err(e)) => break Err(CliError::Connection(format!("tether pty: pty master write: {e}"))),
                                Err(_) => {
                                    // A prefix may already be in the kernel
                                    // buffer; don't retry any of it (or the
                                    // rest) — that would duplicate bytes on
                                    // the tool's side.
                                    dropped_write_bytes += bytes.len() as u64;
                                    if write_drop_limiter.ready() {
                                        eprintln!(
                                            "tether: tool not reading {link}; dropped {dropped_write_bytes} bytes"
                                        );
                                        dropped_write_bytes = 0;
                                    }
                                }
                            }
                        } else if method == "device" {
                            if let Some(p) = params {
                                let kind = p.get("kind").and_then(|s| s.as_str()).unwrap_or("?");
                                eprintln!("tether: [device {kind}]");
                            }
                        }
                    }
                }
            }

            _ = termios_tick.tick() => {
                if reassert_raw_if_needed(slave_raw_fd) {
                    eprintln!(
                        "tether: reasserted raw mode on {link} (a tool set cooked/echo mode; binary data would corrupt)"
                    );
                }
            }
        }
    };

    // Best-effort: don't leave an exec'd tool running against a port that's
    // about to disappear (link removal is handled by `PtyLinkGuard` back in
    // `pty_session`).
    if let Some(c) = child.as_mut() {
        if matches!(c.try_wait(), Ok(None)) {
            let _ = c.start_kill();
            let _ = c.wait().await;
        }
    }
    let _ = sink.close().await;
    outcome
}

async fn sync_until_idle<S>(
    framed: &mut Framed<S, NdjsonCodec>,
    next_id: &mut i64,
    session_id: &str,
    idle_ms: u32,
    timeout_ms: u32,
) -> Result<String, CliError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let started = std::time::Instant::now();
    let timeout_total = Duration::from_millis(timeout_ms as u64);
    let idle = Duration::from_millis(idle_ms as u64);

    let mut accumulated: Vec<u8> = Vec::new();
    let mut last_growth = std::time::Instant::now();

    // Use short expects to gather chunks and decide when idle.
    loop {
        if started.elapsed() >= timeout_total {
            break;
        }
        let remain = timeout_total.saturating_sub(started.elapsed());
        let step = idle.min(remain);
        // expect ".+" from the current cursor — a failure means nothing arrived in this window.
        let p = ExpectParams {
            session_id: session_id.to_string(),
            pattern: "(?s).+".into(),
            regex: true,
            timeout_ms: Some(step.as_millis() as u32),
            strip_ansi: false,
            strip_echo: None,
            from_seq: None,
            max_bytes: None,
            max_output_bytes: None,
        };
        match call(framed, next_id, "expect", serde_json::to_value(p).unwrap()).await {
            Ok(v) => {
                if let Some(b64) = v.get("before").and_then(|s| s.as_str()) {
                    let mut chunk = base64::engine::general_purpose::STANDARD
                        .decode(b64)
                        .unwrap_or_default();
                    accumulated.append(&mut chunk);
                }
                if let Some(m) = v.get("match").and_then(|s| s.as_str()) {
                    accumulated.extend_from_slice(m.as_bytes());
                }
                last_growth = std::time::Instant::now();
            }
            Err(CliError::Timeout(_)) => {
                if last_growth.elapsed() >= idle && !accumulated.is_empty() {
                    break;
                }
                // Nothing yet — keep waiting.
            }
            Err(e) => return Err(e),
        }
    }

    // The last non-empty line is the prompt candidate.
    let s = String::from_utf8_lossy(&accumulated);
    let last = s
        .lines()
        .rfind(|l| !l.trim().is_empty())
        .unwrap_or("")
        .to_string();
    Ok(last)
}

// ---------- Output formatting ----------

fn print_json_or_pairs(v: &Value, force_json: bool) {
    if force_json {
        println!("{}", serde_json::to_string_pretty(v).unwrap_or_default());
        return;
    }
    if let Some(obj) = v.as_object() {
        for (k, val) in obj {
            println!("{k}: {}", val_compact(val));
        }
    } else {
        println!("{}", v);
    }
}

fn print_match_result(v: &Value, force_json: bool) {
    if force_json {
        // Add a decoded `output` field so an LLM can use it directly.
        let mut out = v.clone();
        if let Some(obj) = out.as_object_mut() {
            if let Some(b64) = obj.get("before").and_then(|s| s.as_str()) {
                if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(b64) {
                    let text = String::from_utf8_lossy(&bytes).to_string();
                    obj.insert("output".into(), json!(text));
                }
            }
        }
        println!("{}", serde_json::to_string_pretty(&out).unwrap_or_default());
        return;
    }
    // Human-friendly: response text to stdout, metadata to stderr.
    let before_b64 = v.get("before").and_then(|s| s.as_str()).unwrap_or("");
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(before_b64)
        .unwrap_or_default();
    let text = String::from_utf8_lossy(&bytes);
    print!("{text}");
    if !text.ends_with('\n') {
        println!();
    }
    let truncated = v.get("truncated").and_then(|b| b.as_bool()).unwrap_or(false);
    if truncated || std::io::stderr().is_terminal() {
        let m = v.get("match").and_then(|s| s.as_str()).unwrap_or("");
        let dur = v.get("duration_ms").and_then(|n| n.as_u64());
        let mut summary = format!("[matched: {m:?}");
        if let Some(d) = dur {
            summary.push_str(&format!(", {d}ms"));
        }
        if truncated {
            if let Some(orig) = v.get("original_bytes").and_then(|n| n.as_u64()) {
                summary.push_str(&format!(", truncated from {orig}B"));
            } else {
                summary.push_str(", truncated");
            }
        }
        summary.push(']');
        eprintln!("{summary}");
    }
}

/// Wrap a user command so the device shell brackets its output with two unique
/// markers and reports the command's exit status. Returns `(wrapped_command,
/// end_regex, begin_marker)`.
///
/// Each marker is split by an empty-string quote (`BE""G`) in the *typed* form,
/// so the shell's echo of the command line can never contain the marker
/// contiguously — only the shell's *evaluated* output does. The caller keeps
/// only the bytes *between* the begin marker's line and the end marker, which
/// makes the extraction immune to terminal line-wrapping of the echoed command
/// (a long command that wraps at the device's column width would otherwise leak
/// into the output). The random tag makes an accidental match in real command
/// output effectively impossible.
fn wrap_exec_command(cmd: &str) -> (String, String, String) {
    use uuid::Uuid;
    let tag = Uuid::new_v4().simple().to_string();
    let tag = &tag[..12];
    let begin_print = format!("TETHEREXECBEG{tag}");
    let begin_typed = format!("TETHEREXECBE\"\"G{tag}");
    let end_typed = format!("TETHEREXECEN\"\"D{tag}");
    // `$?` is expanded *inside* the echo argument, so it still reflects
    // `<cmd>`'s status without a temp variable. Dropping the old `__trc=$?`
    // assignment is what makes this work on U-Boot hush (which has no bare
    // variable assignment: `__trc=0` became `Unknown command '__trc=0'` and
    // clobbered `$?`), while remaining correct on POSIX shells.
    let wrapped = format!("echo \"{begin_typed}\"; {cmd}; echo \"{end_typed}=$?\"");
    // Detection stays tolerant: match the marker with *any* non-space status
    // token (a shell that doesn't expand `$?` leaves it literal, e.g. U-Boot's
    // `$__trc`/`$?` or PowerShell's `True`/`False`). The status is parsed as an
    // integer separately in `print_exec_result`; a non-numeric token yields an
    // unknown exit code rather than a hang. `\S+` (not `\S*`) avoids matching
    // the bare `=` before the status byte has arrived.
    let end_re = format!(r"TETHEREXECEND{tag}=(\S+)");
    (wrapped, end_re, begin_print)
}

/// Print an `exec` result and return the device-side exit code, or `None` when
/// the shell did not report a numeric status (e.g. a non-POSIX console left the
/// `$?` token literal). In JSON mode emits `{output, exit_code, duration_ms,
/// truncated}` with `exit_code: null` for the unknown case — never a fabricated
/// `0`. Otherwise the captured output goes to stdout verbatim and a one-line
/// summary goes to stderr when it's a TTY or the command failed; an unknown
/// status also prints a one-line hint pointing at the non-POSIX guide.
/// `begin_marker` brackets the start of the real output (everything before and
/// including its line is the echoed command and is discarded).
fn print_exec_result(v: &Value, begin_marker: &str, force_json: bool) -> Option<u8> {
    let before_b64 = v.get("before").and_then(|s| s.as_str()).unwrap_or("");
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(before_b64)
        .unwrap_or_default();
    let raw = String::from_utf8_lossy(&bytes);
    // Keep only what follows the begin marker's line; this drops the echoed
    // command (even if the device terminal wrapped it across several lines).
    let output = match raw.find(begin_marker) {
        Some(pos) => {
            let rest = &raw[pos + begin_marker.len()..];
            let from = rest.find('\n').map(|i| i + 1).unwrap_or(rest.len());
            rest[from..].to_string()
        }
        None => raw.to_string(),
    };
    // match_text looks like `TETHEREXECEND<tag>=<status>`; pull the trailing
    // token and parse it as an integer. A non-numeric token (a shell that
    // didn't expand `$?`) is an *unknown* status — never silently 0.
    let match_text = v.get("match").and_then(|s| s.as_str()).unwrap_or("");
    let exit_code: Option<u8> = match_text
        .rsplit_once('=')
        .and_then(|(_, n)| n.trim().parse::<i64>().ok())
        .map(|n| n.clamp(0, 255) as u8);
    let truncated = v.get("truncated").and_then(|b| b.as_bool()).unwrap_or(false);
    let duration_ms = v.get("duration_ms").and_then(|n| n.as_u64());

    if force_json {
        let mut obj = serde_json::Map::new();
        obj.insert("output".into(), json!(output));
        // null (not 0) when the device didn't report a numeric status.
        obj.insert("exit_code".into(), json!(exit_code));
        if let Some(d) = duration_ms {
            obj.insert("duration_ms".into(), json!(d));
        }
        obj.insert("truncated".into(), json!(truncated));
        println!(
            "{}",
            serde_json::to_string_pretty(&Value::Object(obj)).unwrap_or_default()
        );
        return exit_code;
    }

    print!("{output}");
    if !output.is_empty() && !output.ends_with('\n') {
        println!();
    }
    match exit_code {
        None => {
            eprintln!(
                "tether: device shell did not report a numeric status \
                 (non-POSIX console?) — see docs/EXEC_NONPOSIX_SHELLS.md"
            );
        }
        Some(code) if code != 0 || truncated || std::io::stderr().is_terminal() => {
            let mut summary = format!("[exit {code}");
            if let Some(d) = duration_ms {
                summary.push_str(&format!(", {d}ms"));
            }
            if truncated {
                match v.get("original_bytes").and_then(|n| n.as_u64()) {
                    Some(orig) => summary.push_str(&format!(", truncated from {orig}B")),
                    None => summary.push_str(", truncated"),
                }
            }
            summary.push(']');
            eprintln!("{summary}");
        }
        Some(_) => {}
    }
    exit_code
}

/// Print the most useful one-line diagnosis for an `exec` timeout, chosen from
/// what (if anything) reached the pre-match buffer. Ordered most-specific
/// first, so an agent gets an actionable next step instead of the misleading
/// "no end-marker seen" when the marker actually printed. `begin_marker` is
/// `TETHEREXECBEG<tag>`, from which the end-marker prefix is derived.
fn exec_timeout_hint(
    timeout_ms: u32,
    buffered: Option<&str>,
    begin_marker: &str,
    personality: &Personality,
    dflag: &str,
    cmd: &str,
) {
    let tag = begin_marker.strip_prefix("TETHEREXECBEG").unwrap_or("");
    let end_prefix = format!("TETHEREXECEND{tag}=");
    let buf = buffered.unwrap_or("");

    // The end marker printed but its status token never parsed (e.g. the shell
    // left `$?` literal, or the value was empty). The command ran; the shell
    // just isn't POSIX. Say so instead of blaming a missing prompt.
    if !tag.is_empty() && buf.contains(&end_prefix) {
        eprintln!(
            "tether: exec: the end marker was seen but the device shell did not report \
             a numeric status (non-POSIX console?) — see docs/EXEC_NONPOSIX_SHELLS.md"
        );
        return;
    }
    // Classic non-hush U-Boot / raw-console symptom.
    if buf.contains("Unknown command") {
        eprintln!(
            "tether: exec timed out after {timeout_ms}ms — the device looks like a U-Boot / \
             raw console (saw \"Unknown command\").\n  \
             Try: tether{dflag} run \"{cmd}\" -u \"=> \" --newline cr\n  \
             or set `shell=uboot` on the device (see -D / daemon config). \
             See docs/EXEC_NONPOSIX_SHELLS.md"
        );
        return;
    }
    // Configured U-Boot but still no marker: most likely a U-Boot built without
    // the hush parser, which can't run `exec` at all.
    if personality.shell == "uboot" {
        eprintln!(
            "tether: exec timed out after {timeout_ms}ms on a shell=uboot device — no end \
             marker seen. A U-Boot built without the hush parser can't run `exec`; fall back to \
             tether{dflag} run \"{cmd}\" -u \"=> \" --newline cr. See docs/EXEC_NONPOSIX_SHELLS.md"
        );
        return;
    }
    // Generic fallback.
    eprintln!(
        "tether: exec timed out after {timeout_ms}ms — no end-marker seen. \
         Is the device at a shell prompt? (exec needs a POSIX-ish shell; for raw consoles use \
         `run`/`send`/`expect`, or set `shell=` on the device — see docs/EXEC_NONPOSIX_SHELLS.md.)"
    );
}

/// Pull responses off `resp_rx` until we see one whose id matches the request
/// we just sent, or `timeout` elapses. Foreign responses (e.g. an in-flight
/// `send` ack from the shell loop) are discarded. Returns `None` on timeout
/// or channel close.
async fn recv_response_for(
    want_id: i64,
    resp_rx: &mut tokio::sync::mpsc::UnboundedReceiver<tether_protocol::Response>,
    timeout: std::time::Duration,
) -> Option<tether_protocol::message::ResponsePayload> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let remain = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remain.is_zero() {
            return None;
        }
        match tokio::time::timeout(remain, resp_rx.recv()).await {
            Ok(Some(resp)) => match &resp.id {
                RpcId::Number(n) if *n == want_id => return Some(resp.payload),
                _ => continue, // foreign response — keep waiting
            },
            _ => return None,
        }
    }
}

fn print_ports(v: &Value, force_json: bool) {
    if force_json {
        println!("{}", serde_json::to_string_pretty(v).unwrap_or_default());
        return;
    }
    let empty = Vec::<Value>::new();
    let ports = v.get("ports").and_then(|p| p.as_array()).unwrap_or(&empty);
    if ports.is_empty() {
        eprintln!("(no ports detected)");
        return;
    }
    for port in ports {
        let path = port.get("path").and_then(|s| s.as_str()).unwrap_or("?");
        let kind = port.get("kind").and_then(|s| s.as_str()).unwrap_or("unknown");
        let mut details: Vec<String> = Vec::new();
        if let Some(s) = port.get("manufacturer").and_then(|s| s.as_str()) {
            details.push(format!("manufacturer={s}"));
        }
        if let Some(s) = port.get("product").and_then(|s| s.as_str()) {
            details.push(format!("product={s}"));
        }
        if let Some(s) = port.get("serial_number").and_then(|s| s.as_str()) {
            details.push(format!("serial={s}"));
        }
        if let (Some(vid), Some(pid)) = (
            port.get("vid").and_then(|s| s.as_str()),
            port.get("pid").and_then(|s| s.as_str()),
        ) {
            details.push(format!("usb={vid}:{pid}"));
        }
        if details.is_empty() {
            println!("{path}  ({kind})");
        } else {
            println!("{path}  ({kind})  {}", details.join(", "));
        }
    }
}

fn print_device_list(v: &Value, force_json: bool) {
    if force_json {
        println!("{}", serde_json::to_string_pretty(v).unwrap_or_default());
        return;
    }
    let empty = Vec::<Value>::new();
    let devices = v.get("devices").and_then(|x| x.as_array()).unwrap_or(&empty);
    let default_id = v.get("default_device").and_then(|s| s.as_str()).unwrap_or("");
    if devices.is_empty() {
        eprintln!("(no devices)");
        return;
    }
    for d in devices {
        let id = d.get("id").and_then(|s| s.as_str()).unwrap_or("?");
        let path = d.get("path").and_then(|s| s.as_str()).unwrap_or("?");
        let baud = d.get("baud").and_then(|n| n.as_u64()).unwrap_or(0);
        let connected = d.get("connected").and_then(|b| b.as_bool()).unwrap_or(false);
        let exp_disc = d
            .get("explicitly_disconnected")
            .and_then(|b| b.as_bool())
            .unwrap_or(false);
        let parity = d.get("parity").and_then(|s| s.as_str()).unwrap_or("none");
        let data_bits = d.get("data_bits").and_then(|n| n.as_u64()).unwrap_or(8);
        let stop_bits = d.get("stop_bits").and_then(|n| n.as_u64()).unwrap_or(1);
        let flow = d.get("flow_control").and_then(|s| s.as_str()).unwrap_or("none");
        let parity_letter = parity.chars().next().map(|c| c.to_ascii_uppercase()).unwrap_or('?');
        let marker = if id == default_id { " (default)" } else { "" };
        let status = if exp_disc {
            "disconnected (explicit)"
        } else if connected {
            "connected"
        } else {
            "disconnected"
        };
        // Only surface a shell tag when it's not the plain-POSIX default, to
        // keep the common case uncluttered.
        let shell = d.get("shell").and_then(|s| s.as_str()).unwrap_or("posix");
        let shell_tag = if shell != "posix" {
            format!("  shell={shell}")
        } else {
            String::new()
        };
        println!(
            "{:10}  {} @ {} {}{}{} flow={}  [{}]{}{}",
            id, path, baud, data_bits, parity_letter, stop_bits, flow, status, shell_tag, marker
        );
    }
}

fn print_modem_status(v: &Value, force_json: bool) {
    if force_json {
        println!("{}", serde_json::to_string_pretty(v).unwrap_or_default());
        return;
    }
    let cts = v.get("cts").and_then(|b| b.as_bool()).unwrap_or(false);
    let dsr = v.get("dsr").and_then(|b| b.as_bool()).unwrap_or(false);
    let ri = v.get("ri").and_then(|b| b.as_bool()).unwrap_or(false);
    let dcd = v.get("dcd").and_then(|b| b.as_bool()).unwrap_or(false);
    let mark = |b| if b { "1" } else { "0" };
    println!("CTS={}  DSR={}  RI={}  DCD={}", mark(cts), mark(dsr), mark(ri), mark(dcd));
}

fn print_device_config(device: &Value, force_json: bool) {
    if force_json {
        println!("{}", serde_json::to_string_pretty(device).unwrap_or_default());
        return;
    }
    let path = device.get("path").and_then(|s| s.as_str()).unwrap_or("?");
    let baud = device.get("baud").and_then(|n| n.as_u64()).unwrap_or(0);
    let data_bits = device.get("data_bits").and_then(|n| n.as_u64()).unwrap_or(8);
    let parity = device.get("parity").and_then(|s| s.as_str()).unwrap_or("none");
    let stop_bits = device.get("stop_bits").and_then(|n| n.as_u64()).unwrap_or(1);
    let flow = device.get("flow_control").and_then(|s| s.as_str()).unwrap_or("none");
    let connected = device.get("connected").and_then(|b| b.as_bool()).unwrap_or(false);
    let parity_letter = parity
        .chars()
        .next()
        .map(|c| c.to_ascii_uppercase())
        .unwrap_or('?');
    println!("path:         {path}");
    println!("baud:         {baud}");
    println!("framing:      {data_bits}{parity_letter}{stop_bits}");
    println!("flow_control: {flow}");
    println!("connected:    {connected}");
    // Console personality (present on daemons that support `-D shell=`).
    if let Some(shell) = device.get("shell").and_then(|s| s.as_str()) {
        println!("shell:        {shell}");
    }
    if let Some(prompt) = device.get("prompt").and_then(|s| s.as_str()) {
        println!("prompt:       {prompt}");
    }
    if let Some(newline) = device.get("newline").and_then(|s| s.as_str()) {
        println!("newline:      {newline}");
    }
}

/// Append a line terminator if requested.
fn apply_newline(data: String, kind: &str) -> String {
    match kind {
        "lf" => format!("{data}\n"),
        "cr" => format!("{data}\r"),
        "crlf" => format!("{data}\r\n"),
        _ => data, // "none" or unknown
    }
}

fn val_compact(v: &Value) -> String {
    if v.is_object() || v.is_array() {
        serde_json::to_string(v).unwrap_or_default()
    } else {
        v.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn select_port_choices_keeps_usb_cu_drops_tty_and_nonusb() {
        // Mirrors what macOS `available_ports()` returns: cu/tty pairs plus
        // virtual non-USB entries. Only the USB call-out (`cu.*`) nodes survive.
        let input = vec![
            ("/dev/cu.debug-console".to_string(), None),
            ("/dev/tty.debug-console".to_string(), None),
            (
                "/dev/cu.usbserial-0001".to_string(),
                Some("CP2102 USB to UART".to_string()),
            ),
            (
                "/dev/tty.usbserial-0001".to_string(),
                Some("CP2102 USB to UART".to_string()),
            ),
            ("/dev/cu.usbserial-3".to_string(), Some(String::new())),
            ("/dev/tty.usbserial-3".to_string(), Some(String::new())),
        ];
        let got = select_port_choices(input);
        let paths: Vec<&str> = got.iter().map(|c| c.path.as_str()).collect();
        assert_eq!(paths, ["/dev/cu.usbserial-0001", "/dev/cu.usbserial-3"]);
        // Label carries the description when present, bare path otherwise.
        assert_eq!(got[0].label, "/dev/cu.usbserial-0001  (CP2102 USB to UART)");
        assert_eq!(got[1].label, "/dev/cu.usbserial-3");
    }

    #[test]
    fn wrap_exec_command_is_unified_and_hush_safe() {
        let (wrapped, end_re, begin) = wrap_exec_command("mdio list");
        // The old `__trc=$?` temp assignment is gone — that's what broke hush.
        assert!(
            !wrapped.contains("__trc"),
            "wrapper must not use a temp var: {wrapped}"
        );
        // Status is captured by expanding `$?` inside the end echo's argument.
        assert!(wrapped.contains("=$?\""), "wrapper must echo =$?: {wrapped}");
        assert!(wrapped.contains("mdio list"));
        // Split-quote markers in the *typed* form, contiguous in the regex.
        assert!(wrapped.contains("TETHEREXECBE\"\"G"));
        assert!(wrapped.contains("TETHEREXECEN\"\"D"));
        assert!(begin.starts_with("TETHEREXECBEG"));
        // Tolerant end regex: any non-space status token, not just digits.
        assert!(end_re.contains("=(\\S+)"), "end regex should be tolerant: {end_re}");
    }

    #[test]
    fn print_exec_result_reports_unknown_status_as_none() {
        // A non-numeric status token (shell left `$?` literal) must be None,
        // never a fabricated 0.
        let tag = "abc123abc123";
        let before = base64::engine::general_purpose::STANDARD
            .encode(format!("TETHEREXECBEG{tag}\nhello\n"));
        let v = json!({
            "before": before,
            "match": format!("TETHEREXECEND{tag}=$__trc"),
        });
        assert_eq!(
            print_exec_result(&v, &format!("TETHEREXECBEG{tag}"), true),
            None
        );

        // A numeric status parses back to that code.
        let v_ok = json!({
            "before": base64::engine::general_purpose::STANDARD
                .encode(format!("TETHEREXECBEG{tag}\nhello\n")),
            "match": format!("TETHEREXECEND{tag}=42"),
        });
        assert_eq!(
            print_exec_result(&v_ok, &format!("TETHEREXECBEG{tag}"), true),
            Some(42)
        );
    }

    #[test]
    fn select_port_choices_keeps_linux_ttyusb() {
        // Linux nodes are `/dev/ttyUSB0` (no dot) — must NOT be filtered.
        let input = vec![
            ("/dev/ttyUSB0".to_string(), Some("FT232".to_string())),
            ("/dev/ttyACM0".to_string(), Some(String::new())),
        ];
        let paths: Vec<String> = select_port_choices(input)
            .into_iter()
            .map(|c| c.path)
            .collect();
        assert_eq!(paths, ["/dev/ttyUSB0", "/dev/ttyACM0"]);
    }

    #[test]
    fn parse_reset_steps_esp32_preset() {
        let steps = parse_reset_steps("dtr=0 rts=1 wait=100 dtr=1 rts=0 wait=50 dtr=0").unwrap();
        assert_eq!(
            steps,
            vec![
                ResetStep::Dtr(false),
                ResetStep::Rts(true),
                ResetStep::Wait(100),
                ResetStep::Dtr(true),
                ResetStep::Rts(false),
                ResetStep::Wait(50),
                ResetStep::Dtr(false),
            ]
        );
    }

    #[test]
    fn parse_reset_steps_accepts_aliases_and_separators() {
        // commas, on/off/high/low, and wait aliases all work.
        let steps = parse_reset_steps("rts=on,delay=10,rts=off").unwrap();
        assert_eq!(
            steps,
            vec![ResetStep::Rts(true), ResetStep::Wait(10), ResetStep::Rts(false)]
        );
        assert_eq!(parse_reset_steps("dtr=low").unwrap(), vec![ResetStep::Dtr(false)]);
    }

    #[test]
    fn parse_reset_steps_rejects_garbage() {
        assert!(parse_reset_steps("").is_err()); // empty
        assert!(parse_reset_steps("dtr").is_err()); // no '='
        assert!(parse_reset_steps("dtr=maybe").is_err()); // bad state
        assert!(parse_reset_steps("wait=soon").is_err()); // bad ms
        assert!(parse_reset_steps("cts=1").is_err()); // unknown line
    }

    #[test]
    fn command_needs_device_excludes_inspection_commands() {
        assert!(!command_needs_device(&Cmd::Status));
        assert!(!command_needs_device(&Cmd::ListDevices));
        assert!(!command_needs_device(&Cmd::Ports));
        assert!(!command_needs_device(&Cmd::Agents));
        assert!(command_needs_device(&Cmd::Tail { from: "now".into() }));
        assert!(command_needs_device(&Cmd::Lines));
    }

    #[cfg(unix)]
    #[test]
    fn substitute_link_placeholder_replaces_all_occurrences() {
        let args = vec!["minicom".to_string(), "-D".to_string(), "{}".to_string()];
        assert_eq!(
            substitute_link_placeholder(&args, "/tmp/x.pty"),
            vec!["minicom", "-D", "/tmp/x.pty"]
        );
        // `{}` embedded inside a larger token is also substituted.
        let args = vec!["--device={}".to_string()];
        assert_eq!(
            substitute_link_placeholder(&args, "/tmp/x.pty"),
            vec!["--device=/tmp/x.pty"]
        );
    }

    #[cfg(unix)]
    #[test]
    fn substitute_link_placeholder_appends_when_no_placeholder() {
        let args = vec!["python3".to_string(), "script.py".to_string()];
        assert_eq!(
            substitute_link_placeholder(&args, "/tmp/x.pty"),
            vec!["python3", "script.py", "/tmp/x.pty"]
        );
    }

    #[cfg(unix)]
    #[test]
    fn default_pty_link_uses_id_and_slot() {
        assert_eq!(default_pty_link("board0", 1), "/tmp/tether-board0-1.pty");
        assert_eq!(default_pty_link("port", 3), "/tmp/tether-port-3.pty");
    }

    #[cfg(unix)]
    #[test]
    fn pid_sidecar_staleness_decision() {
        // Missing/unreadable sidecar: can't tell — caller treats as alive.
        assert_eq!(pid_sidecar_is_stale(None, |_| true), None);
        // Non-numeric contents: also can't tell.
        assert_eq!(pid_sidecar_is_stale(Some("not-a-pid"), |_| true), None);
        // Probe says alive → not stale.
        assert_eq!(pid_sidecar_is_stale(Some("123"), |_| true), Some(false));
        // Probe says gone → stale, safe to reclaim.
        assert_eq!(pid_sidecar_is_stale(Some("123"), |_| false), Some(true));
    }
}
