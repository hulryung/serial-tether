//! Focused integration tests for the explicit `lock` / `unlock` RPCs — the
//! exclusive, session-held writer lock that (unlike `run`'s internal
//! transient hold) also gates plain `send` from other sessions. See
//! docs/PROTOCOL.md's writer-lock section for the full contract.
//!
//! These speak raw NDJSON JSON-RPC directly over the daemon's UDS socket
//! instead of going through the `tether` CLI, since `lock`/`unlock` don't
//! have client-side subcommands yet. The PTY/daemon-spawn helpers below are
//! trimmed copies of the ones in `tests/integration.rs` — see that file for
//! the fuller machinery this borrows from (each `tests/*.rs` file compiles
//! as its own crate, so helpers aren't shared).

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use tether_protocol::error::ErrorCode;

const TETHERD: &str = env!("CARGO_BIN_EXE_tetherd");

static TEST_SEQ: AtomicU32 = AtomicU32::new(0);

fn unique_id() -> u32 {
    TEST_SEQ.fetch_add(1, Ordering::SeqCst)
}

fn unique_socket() -> String {
    format!("/tmp/tetherd-lock-it-{}-{}.sock", std::process::id(), unique_id())
}

// ---------- PTY helper (trimmed copy of tests/integration.rs's spawn_pty) ----------

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
        "/tmp/tetherd-lock-it-socat-{}-{}.log",
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
    std::thread::sleep(Duration::from_millis(100));
    Pty { child, path }
}

// ---------- Daemon helper (trimmed copy; single-device only) ----------

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

fn spawn_daemon(pty_path: &str) -> Daemon {
    let socket = unique_socket();
    let mut child = Command::new(TETHERD)
        .arg("-s")
        .arg(&socket)
        .arg("-D")
        .arg(pty_path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn tetherd");

    // Wait for the socket to appear (daemon binds it after opening the device).
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if Path::new(&socket).exists() {
            // Give it another beat to actually accept connections.
            std::thread::sleep(Duration::from_millis(80));
            return Daemon { child, socket };
        }
        std::thread::sleep(Duration::from_millis(40));
    }
    let _ = child.kill();
    let _ = child.wait();
    panic!("tetherd failed to bind {socket} within 5s");
}

// ---------- Raw NDJSON JSON-RPC session ----------

/// One raw JSON-RPC connection to the daemon's UDS socket — no `tether` CLI
/// involved, so we can drive `lock`/`unlock` directly.
struct RawSession {
    stream: UnixStream,
    reader: BufReader<UnixStream>,
    next_id: i64,
}

impl RawSession {
    fn connect(socket: &str) -> Self {
        let stream = UnixStream::connect(socket).expect("connect uds");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("set read timeout");
        let reader = BufReader::new(stream.try_clone().expect("clone uds stream"));
        Self { stream, reader, next_id: 1 }
    }

    /// Send one request and return the response matching its id (either
    /// `{"result":...}` or `{"error":...}`), skipping over any interleaved
    /// `data`/`lag`/`device` notifications on the same connection.
    fn call(&mut self, method: &str, params: serde_json::Value) -> serde_json::Value {
        let id = self.next_id;
        self.next_id += 1;
        let req = serde_json::json!({"jsonrpc":"2.0","id":id,"method":method,"params":params});
        let mut line = serde_json::to_string(&req).unwrap();
        line.push('\n');
        self.stream.write_all(line.as_bytes()).expect("write request");
        loop {
            let mut buf = String::new();
            let n = self.reader.read_line(&mut buf).expect("read response");
            assert!(n > 0, "daemon closed the connection unexpectedly");
            let v: serde_json::Value =
                serde_json::from_str(buf.trim_end()).expect("parse ndjson line");
            if v.get("id").and_then(|x| x.as_i64()) == Some(id) {
                return v;
            }
            // Not our response — an unrelated notification. Keep reading.
        }
    }

    /// `hello` + `attach(mode:"rw")`. Returns the session id.
    fn attach_rw(&mut self) -> String {
        let hello = self.call(
            "hello",
            serde_json::json!({
                "protocol_version": tether_protocol::PROTOCOL_VERSION,
                "client": {"name": "lock-test", "version": "0", "kind": "agent"},
            }),
        );
        assert!(hello.get("result").is_some(), "hello failed: {hello:?}");
        let attach = self.call("attach", serde_json::json!({"mode": "rw"}));
        attach["result"]["session_id"]
            .as_str()
            .unwrap_or_else(|| panic!("attach failed: {attach:?}"))
            .to_string()
    }
}

fn error_code(resp: &serde_json::Value) -> i64 {
    resp["error"]["code"]
        .as_i64()
        .unwrap_or_else(|| panic!("expected an error response, got {resp:?}"))
}

// ---------- Tests ----------

#[test]
fn exclusive_lock_gates_send_from_other_sessions() {
    let pty = spawn_pty();
    let d = spawn_daemon(&pty.path);

    let mut a = RawSession::connect(&d.socket);
    let a_session = a.attach_rw();
    let mut b = RawSession::connect(&d.socket);
    let b_session = b.attach_rw();

    // A takes the exclusive lock.
    let locked = a.call("lock", serde_json::json!({"session_id": a_session}));
    assert_eq!(
        locked["result"]["locked"].as_bool(),
        Some(true),
        "lock failed: {locked:?}"
    );

    // B's plain `send` must be rejected while A holds it exclusively.
    let send = b.call(
        "send",
        serde_json::json!({"session_id": b_session, "data_text": "x"}),
    );
    assert_eq!(
        error_code(&send),
        ErrorCode::LockContention.as_i32() as i64,
        "expected lock_contention, got {send:?}"
    );

    // A releases the lock.
    let unlocked = a.call("unlock", serde_json::json!({"session_id": a_session}));
    assert_eq!(
        unlocked["result"]["unlocked"].as_bool(),
        Some(true),
        "unlock failed: {unlocked:?}"
    );

    // Now B's `send` goes through.
    let send2 = b.call(
        "send",
        serde_json::json!({"session_id": b_session, "data_text": "x"}),
    );
    assert!(
        send2.get("result").is_some(),
        "send should succeed after unlock: {send2:?}"
    );
}

#[test]
fn unlock_by_non_holder_is_rejected() {
    let pty = spawn_pty();
    let d = spawn_daemon(&pty.path);

    let mut a = RawSession::connect(&d.socket);
    let a_session = a.attach_rw();
    let mut b = RawSession::connect(&d.socket);
    let b_session = b.attach_rw();

    let locked = a.call("lock", serde_json::json!({"session_id": a_session}));
    assert_eq!(locked["result"]["locked"].as_bool(), Some(true));

    // B (not the holder) can't unlock it.
    let unlock_attempt = b.call("unlock", serde_json::json!({"session_id": b_session}));
    assert_eq!(
        error_code(&unlock_attempt),
        ErrorCode::LockContention.as_i32() as i64
    );

    // A can still release it.
    let unlocked = a.call("unlock", serde_json::json!({"session_id": a_session}));
    assert_eq!(unlocked["result"]["unlocked"].as_bool(), Some(true));
}

#[test]
fn connection_teardown_releases_the_lock() {
    let pty = spawn_pty();
    let d = spawn_daemon(&pty.path);

    {
        let mut a = RawSession::connect(&d.socket);
        let a_session = a.attach_rw();
        let locked = a.call("lock", serde_json::json!({"session_id": a_session}));
        assert_eq!(locked["result"]["locked"].as_bool(), Some(true));
        // `a`'s connection drops here without an explicit `unlock` — like a
        // flashing client that crashed mid-flash.
    }
    // Give the daemon a moment to notice the closed connection and run
    // its teardown (which releases any lock the departing session held).
    std::thread::sleep(Duration::from_millis(300));

    let mut b = RawSession::connect(&d.socket);
    let b_session = b.attach_rw();
    let send = b.call(
        "send",
        serde_json::json!({"session_id": b_session, "data_text": "x"}),
    );
    assert!(
        send.get("result").is_some(),
        "send should succeed once the departed session's lock is torn down: {send:?}"
    );
}
