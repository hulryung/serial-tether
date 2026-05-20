//! End-to-end integration tests.
//!
//! Each test spawns its own `tetherd` (with a unique UDS path) backed by
//! `socat`-created PTY pair(s), runs the `tether` CLI against it, and
//! asserts on the JSON output / exit code. Daemons and PTY children get
//! cleaned up via Drop guards even if a test panics.
//!
//! Tests run in parallel by default; isolation comes from the per-test
//! socket / PTY paths. If you need to debug, set `TETHERD_LOG=stderr` —
//! the daemon stderr is redirected to /dev/null otherwise.

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

const TETHERD: &str = env!("CARGO_BIN_EXE_tetherd");
const TETHER: &str = env!("CARGO_BIN_EXE_tether");

static TEST_SEQ: AtomicU32 = AtomicU32::new(0);

fn unique_id() -> u32 {
    TEST_SEQ.fetch_add(1, Ordering::SeqCst)
}

fn unique_socket() -> String {
    format!(
        "/tmp/tetherd-it-{}-{}.sock",
        std::process::id(),
        unique_id()
    )
}

// ---------- PTY helper ----------

/// One end of a socat-created PTY pair. Drop kills socat, releasing both
/// pty endpoints.
struct Pty {
    child: Child,
    pub path: String,
}

impl Drop for Pty {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn spawn_pty() -> Pty {
    let stderr_path = format!(
        "/tmp/tetherd-it-socat-{}-{}.log",
        std::process::id(),
        unique_id()
    );
    let stderr_file = std::fs::File::create(&stderr_path).expect("create socat log");
    let child = Command::new("socat")
        .args(["-d", "-d", "pty,raw,echo=0", "pty,raw,echo=0"])
        .stdout(Stdio::null())
        .stderr(Stdio::from(stderr_file))
        .spawn()
        .expect("spawn socat — is it installed?");

    // socat prints PTY paths to stderr. Poll until we see at least one.
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut path: Option<String> = None;
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
        if let Ok(content) = std::fs::read_to_string(&stderr_path) {
            for line in content.lines() {
                if let Some(idx) = line.find("PTY is ") {
                    let p = line[idx + "PTY is ".len()..].trim();
                    if p.starts_with("/dev/") && p.len() > 5 {
                        path = Some(p.to_string());
                        break;
                    }
                }
            }
            if path.is_some() {
                break;
            }
        }
    }
    let _ = std::fs::remove_file(&stderr_path);
    let path = path.expect("socat to print at least one /dev/pts path within 2s");
    // Tiny grace period for the device node to settle.
    std::thread::sleep(Duration::from_millis(100));
    Pty { child, path }
}

// ---------- Daemon helper ----------

struct Daemon {
    child: Child,
    pub socket: String,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.socket);
    }
}

/// Spawn a daemon with the given device specs (each one a `-D <SPEC>`
/// argument: bare path, `id=path`, or `id=path,key=value,...`).
fn spawn_daemon(specs: &[String]) -> Daemon {
    let socket = unique_socket();
    let mut cmd = Command::new(TETHERD);
    cmd.arg("-s").arg(&socket);
    for spec in specs {
        cmd.arg("-D").arg(spec);
    }
    let child = match std::env::var("TETHERD_LOG").as_deref() {
        Ok("stderr") => cmd.stdout(Stdio::null()).stderr(Stdio::inherit()),
        _ => cmd.stdout(Stdio::null()).stderr(Stdio::null()),
    }
    .spawn()
    .expect("spawn tetherd");

    // Wait for the socket to appear (daemon binds it after opening every device).
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if Path::new(&socket).exists() {
            // Give it another beat to actually accept connections.
            std::thread::sleep(Duration::from_millis(80));
            return Daemon { child, socket };
        }
        std::thread::sleep(Duration::from_millis(40));
    }
    panic!("tetherd failed to bind {socket} within 5s");
}

// ---------- tether CLI helpers ----------

/// Run `tether -s <daemon> <args...> --json`, expect JSON-parseable stdout.
fn tether_json(d: &Daemon, args: &[&str]) -> serde_json::Value {
    let output = Command::new(TETHER)
        .arg("-s")
        .arg(&d.socket)
        .args(args)
        .arg("--json")
        .output()
        .expect("run tether");
    let stdout = String::from_utf8_lossy(&output.stdout);
    serde_json::from_str(&stdout).unwrap_or_else(|e| {
        panic!(
            "tether {args:?} → bad JSON ({e})\nstdout:\n{stdout}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

/// Run `tether -s <daemon> <args...>`. Returns (exit_code, stdout, stderr).
fn tether_exit(d: &Daemon, args: &[&str]) -> (i32, String, String) {
    let output = Command::new(TETHER)
        .arg("-s")
        .arg(&d.socket)
        .args(args)
        .output()
        .expect("run tether");
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).to_string(),
        String::from_utf8_lossy(&output.stderr).to_string(),
    )
}

// ---------- Tests ----------

#[test]
fn single_device_status_returns_device_info() {
    let pty = spawn_pty();
    let d = spawn_daemon(&[pty.path.clone()]);

    let v = tether_json(&d, &["status"]);
    let device = v.get("device").expect("status.device");
    assert_eq!(
        device.get("path").and_then(|s| s.as_str()),
        Some(pty.path.as_str())
    );
    assert_eq!(device.get("connected").and_then(|b| b.as_bool()), Some(true));

    // Default device should equal the basename-derived id.
    let default = v.get("default_device").and_then(|s| s.as_str()).unwrap();
    assert!(!default.is_empty());

    // The new `devices` array has exactly one entry, matching `device`.
    let devices = v.get("devices").and_then(|d| d.as_array()).unwrap();
    assert_eq!(devices.len(), 1);
}

#[test]
fn multi_device_list_devices_returns_both() {
    let pty_a = spawn_pty();
    let pty_b = spawn_pty();
    let d = spawn_daemon(&[
        format!("board0={}", pty_a.path),
        format!("board1={},baud=9600", pty_b.path),
    ]);

    let v = tether_json(&d, &["list-devices"]);
    let devices = v
        .get("devices")
        .and_then(|x| x.as_array())
        .expect("list-devices.devices array");
    assert_eq!(devices.len(), 2);

    let mut ids: Vec<&str> = devices
        .iter()
        .map(|d| d.get("id").and_then(|s| s.as_str()).unwrap_or(""))
        .collect();
    ids.sort();
    assert_eq!(ids, vec!["board0", "board1"]);

    // default_device is the first -D (board0).
    assert_eq!(
        v.get("default_device").and_then(|s| s.as_str()),
        Some("board0")
    );

    // Per-device baud override took effect.
    let board1 = devices
        .iter()
        .find(|d| d.get("id").and_then(|s| s.as_str()) == Some("board1"))
        .unwrap();
    assert_eq!(
        board1.get("baud").and_then(|n| n.as_u64()),
        Some(9600),
        "board1 should have baud=9600 from -D inline override"
    );
}

#[test]
fn multi_device_ambiguous_returns_32015() {
    let pty_a = spawn_pty();
    let pty_b = spawn_pty();
    let d = spawn_daemon(&[
        format!("board0={}", pty_a.path),
        format!("board1={}", pty_b.path),
    ]);

    // `break` requires a device target; without --device, multi-device
    // daemons return AmbiguousDevice (-32015).
    let (code, _stdout, stderr) = tether_exit(&d, &["break"]);
    assert_ne!(code, 0, "expected non-zero exit");
    assert!(
        stderr.contains("-32015") || stderr.contains("ambiguous"),
        "expected ambiguous-device error, got stderr: {stderr}"
    );

    // With explicit --device it should succeed (or fail with Unsupported,
    // which is a different test). Just verify the AmbiguousDevice path is
    // gone.
    let (_, _, stderr2) = tether_exit(&d, &["-d", "board0", "break"]);
    assert!(
        !stderr2.contains("-32015"),
        "AmbiguousDevice should not fire when --device is given"
    );
}

#[test]
fn tio_control_on_pty_returns_unsupported() {
    let pty = spawn_pty();
    let d = spawn_daemon(&[pty.path.clone()]);

    // PTY backend (Fd) does not implement BREAK / DTR / RTS / modem-status.
    // The daemon should answer -32007 unsupported_serial_op for each.
    for sub in [
        vec!["break"],
        vec!["dtr", "on"],
        vec!["rts", "on"],
        vec!["lines"],
    ] {
        let (code, _stdout, stderr) = tether_exit(&d, &sub);
        assert_ne!(code, 0, "tether {sub:?} should fail on PTY");
        assert!(
            stderr.contains("-32007") || stderr.contains("unsupported"),
            "tether {sub:?} → expected unsupported_serial_op, got: {stderr}"
        );
    }
}

#[test]
fn disconnect_connect_cycle_toggles_explicit_flag() {
    let pty = spawn_pty();
    let d = spawn_daemon(&[pty.path.clone()]);

    // Initial state.
    let v = tether_json(&d, &["list-devices"]);
    let dev = &v["devices"][0];
    assert_eq!(dev["connected"].as_bool(), Some(true));
    assert_eq!(dev["explicitly_disconnected"].as_bool(), Some(false));

    // Disconnect.
    let _ = tether_json(&d, &["disconnect"]);
    std::thread::sleep(Duration::from_millis(250));

    let v = tether_json(&d, &["list-devices"]);
    let dev = &v["devices"][0];
    assert_eq!(
        dev["explicitly_disconnected"].as_bool(),
        Some(true),
        "after disconnect, explicitly_disconnected should be true"
    );
    assert_eq!(
        dev["connected"].as_bool(),
        Some(false),
        "after disconnect, connected should be false"
    );

    // Connect.
    let _ = tether_json(&d, &["connect"]);
    std::thread::sleep(Duration::from_millis(400));

    let v = tether_json(&d, &["list-devices"]);
    let dev = &v["devices"][0];
    assert_eq!(
        dev["explicitly_disconnected"].as_bool(),
        Some(false),
        "after connect, explicitly_disconnected should clear"
    );
    assert_eq!(
        dev["connected"].as_bool(),
        Some(true),
        "after connect, port should be reopened"
    );
}

#[test]
fn path_shorthand_no_dash_d_required() {
    // `tether /dev/<pty> status` should auto-spawn an embedded daemon and
    // succeed without requiring `-D` from the user.
    let pty = spawn_pty();

    let output = Command::new(TETHER)
        .arg(&pty.path)
        .arg("status")
        .arg("--json")
        .output()
        .expect("run tether <PATH> status");

    assert!(
        output.status.success(),
        "tether {} status failed (code {:?})\nstderr:\n{}",
        pty.path,
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );

    let v: serde_json::Value = serde_json::from_str(&String::from_utf8_lossy(&output.stdout))
        .expect("status output is valid JSON");
    let device = v.get("device").expect("status.device");
    assert_eq!(
        device.get("path").and_then(|s| s.as_str()),
        Some(pty.path.as_str())
    );
    assert_eq!(device.get("connected").and_then(|b| b.as_bool()), Some(true));
}

#[test]
fn standalone_redirects_to_existing_daemon() {
    // Simulates the AI-agent footgun: a long-lived `tetherd` is already
    // managing a device, then someone invokes `tether -D <PATH>` (or the
    // bare-path shorthand). Without auto-redirect, two daemons would race
    // for the port. With auto-redirect we expect the second invocation to
    // attach to the existing daemon as a client and pick up the right
    // device_id automatically.
    let pty = spawn_pty();
    let d = spawn_daemon(&[format!("board0={}", pty.path)]);

    let output = Command::new(TETHER)
        .arg("-D")
        .arg(&pty.path)
        .arg("status")
        .arg("--json")
        .output()
        .expect("run tether -D <PATH> status");

    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(
        output.status.success(),
        "tether -D <existing> status failed:\nstderr:\n{stderr}"
    );

    // The friendly redirect message landed.
    assert!(
        stderr.contains("already managed") && stderr.contains("attaching as a client"),
        "expected redirect message, got stderr:\n{stderr}"
    );

    // It actually connected to OUR daemon (board0 in id), not a freshly
    // spawned one (which would have used the basename `ttysNNN` instead).
    let v: serde_json::Value =
        serde_json::from_str(&stdout).expect("status output is valid JSON");
    assert_eq!(
        v.get("default_device").and_then(|s| s.as_str()),
        Some("board0"),
        "should have routed to existing daemon (default_device=board0), got: {v}"
    );

    // Sanity: the existing daemon is still alive and reachable directly.
    let v2 = tether_json(&d, &["status"]);
    assert_eq!(
        v2.get("default_device").and_then(|s| s.as_str()),
        Some("board0")
    );
}

#[test]
fn standalone_with_tcp_exposes_remote_attachable_listener() {
    // `tether <PATH> --tcp=127.0.0.1:<port> --auth-token <token> status`
    // should: spawn an embedded daemon, bind the TCP listener, succeed the
    // status RPC, and print the listener info on stderr so a remote
    // attacher can find it. We also verify a separate `tether -s
    // tcp://...` client can attach to the same daemon while it's running.
    let pty = spawn_pty();
    // Pick a port unlikely to collide with whatever else is running.
    let port: u16 = 5559 + (std::process::id() % 100) as u16;
    let bind = format!("127.0.0.1:{port}");
    let token = "it-token";

    // Run a longer-living client (`tail` blocks reading the buffer) so
    // the embedded daemon stays up while we attach from a second client.
    let mut tail = Command::new(TETHER)
        .arg(&pty.path)
        .arg(format!("--tcp={bind}"))
        .arg("--auth-token").arg(token)
        .arg("tail")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn tether --tcp tail");

    // The embedded daemon has to spawn, open the PTY, bind the UDS,
    // bind TCP, accept hello + status. CI under load can take >800ms.
    // Poll-and-retry instead of a fixed sleep so the test stays fast
    // when the host is idle and tolerant when it's not.
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut out = None;
    while Instant::now() < deadline {
        let attempt = Command::new(TETHER)
            .arg("-s").arg(format!("tcp://{bind}"))
            .arg("--auth-token").arg(token)
            .arg("status")
            .arg("--json")
            .output()
            .expect("run remote-style status");
        if attempt.status.success() {
            out = Some(attempt);
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let out = out.unwrap_or_else(|| {
        panic!("remote-style status never succeeded within 5s — TCP listener didn't come up");
    });

    // Tear down. We have to clean up both the shell client (`tail`) and
    // its grandchild `tetherd` (the embedded daemon). Rust default
    // signal handling means a SIGTERM to `tether` just terminates the
    // process without unwinding — `DaemonGuard::drop` never runs and the
    // grandchild leaks, polluting later test runs.
    //
    // Strategy:
    //   1. SIGKILL the client. We don't care if it unwinds; we'll reap
    //      the grandchild ourselves below.
    //   2. Find any `tetherd` process tagged with our PTY path and
    //      SIGTERM it directly (matching by `-D <pty.path>` is unique
    //      to this test instance — socat hands out fresh /dev/ttysNNN
    //      per spawn).
    let _ = tail.kill();
    let _ = tail.wait();

    let _ = Command::new("pkill")
        .args(["-TERM", "-f", &format!("tetherd -D {}", pty.path)])
        .status();
    // Brief grace period for the daemon to honor SIGTERM and unlink its
    // UDS socket. Followed by SIGKILL as a paranoid backstop.
    std::thread::sleep(Duration::from_millis(300));
    let _ = Command::new("pkill")
        .args(["-KILL", "-f", &format!("tetherd -D {}", pty.path)])
        .status();

    assert!(
        out.status.success(),
        "remote-style status failed:\nstderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&out.stdout))
            .expect("status JSON parses");
    assert_eq!(
        v["device"]["path"].as_str(),
        Some(pty.path.as_str()),
        "remote client should see the embedded daemon's device"
    );
    assert_eq!(v["device"]["connected"].as_bool(), Some(true));
}

#[test]
fn ports_handler_returns_array() {
    // `tether ports` calls the daemon's list_ports RPC. Smoke-check it
    // returns an array (may be empty in restricted environments — both
    // are fine, we just want the shape to be stable).
    let pty = spawn_pty();
    let d = spawn_daemon(&[pty.path.clone()]);
    let v = tether_json(&d, &["ports"]);
    assert!(v.get("ports").and_then(|p| p.as_array()).is_some());
}
