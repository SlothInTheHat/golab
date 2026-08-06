//! The live workspace: what the daemon pushes, and what it stops pushing.
//!
//! Two claims here are load-bearing and easy to regress.
//!
//! First, that a browser can paint from **one frame**. If the snapshot were
//! not sent before anything else, the page would have to fetch on connect, and
//! the polling this phase removed would grow straight back.
//!
//! Second, that a burst of events produces **one** snapshot rather than one
//! per event. The dashboard used to call `/api/status` from `ws.onmessage`, so
//! two hundred events meant two hundred requests, arriving exactly when the
//! server was busiest. Coalescing in the pump is the fix, and this is the test
//! that keeps it fixed.
//!
//! # The websocket client
//!
//! Hand-rolled, and deliberately so: the only alternative is a dependency for
//! one test file, and the surface needed is "connect, read text frames". Same
//! call as the JSON-RPC framing in `atlas-mcp` — a server frame is unmasked,
//! so reading one is a length prefix and a copy.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

fn atlas() -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_atlas"));
    c.env_remove("ATLAS_AGENT");
    c
}

fn run(dir: &Path, args: &[&str]) -> Output {
    atlas()
        .current_dir(dir)
        .args(args)
        .output()
        .expect("failed to run atlas")
}

fn ok(dir: &Path, args: &[&str]) -> String {
    let out = run(dir, args);
    assert!(
        out.status.success(),
        "atlas {args:?} failed ({:?}):\n{}\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).to_string()
}

/// A served workspace with something in it, on a port nobody else is using.
struct Daemon {
    child: Child,
    port: u16,
    _dir: tempfile::TempDir,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Ports are picked by binding one and letting the OS choose, then releasing
/// it. A fixed port would make two test binaries running at once flaky.
fn free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    l.local_addr().expect("addr").port()
}

fn serve() -> Daemon {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let w = |rel: &str, body: &str| {
        let p = d.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    };
    w("api/package.json", r#"{"name":"api","version":"1.0.0"}"#);
    w(
        "api/src/routes.ts",
        "import { record } from '../../lib/src/ledger';\n\
         export function registerRoutes(app) { app.post('/payments', createPayment); }\n\
         export function createPayment(req) { return record(req.amount); }\n",
    );
    w("lib/package.json", r#"{"name":"lib","version":"1.0.0"}"#);
    w("lib/src/ledger.ts", "export function record(x) { return x; }\n");

    ok(d, &["init"]);
    ok(d, &["index"]);
    ok(d, &["--agent", "alice", "agent", "register", "alice", "--kind", "cursor"]);
    ok(d, &["--agent", "alice", "lease", "acquire", "createPayment"]);

    let port = free_port();
    // `--no-watch` is not a flag; the watcher is harmless here and keeps the
    // test honest about what `atlas serve` actually does.
    let child = atlas()
        .current_dir(d)
        .args(["serve", "--port", &port.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn daemon");

    // Wait for the socket rather than sleeping a guessed amount.
    let mut child = child;
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return Daemon {
                child,
                port,
                _dir: dir,
            };
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    // Reap it before failing: a panic here skips `Daemon::drop`, and a leaked
    // daemon holds the binary open and breaks the *next* `cargo build`.
    let _ = child.kill();
    let _ = child.wait();
    panic!("daemon never came up on port {port}");
}

/// The smallest websocket client that can read text frames.
struct Ws {
    stream: BufReader<TcpStream>,
}

impl Ws {
    fn connect(port: u16) -> Ws {
        let mut raw = TcpStream::connect(("127.0.0.1", port)).expect("connect");
        raw.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        // A fixed key is fine: we never verify the server's Sec-WebSocket-Accept,
        // which is the only thing it feeds.
        write!(
            raw,
            "GET /ws HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nUpgrade: websocket\r\n\
             Connection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
             Sec-WebSocket-Version: 13\r\n\r\n"
        )
        .expect("handshake");

        let mut stream = BufReader::new(raw);
        let mut line = String::new();
        stream.read_line(&mut line).expect("status line");
        assert!(
            line.contains("101"),
            "expected a protocol upgrade, got: {line}"
        );
        // Drain the rest of the response headers.
        loop {
            let mut h = String::new();
            stream.read_line(&mut h).expect("header");
            if h == "\r\n" || h.is_empty() {
                break;
            }
        }
        Ws { stream }
    }

    /// One text frame, or `None` when the read times out.
    ///
    /// Server frames are never masked, so this is a header, a length, and a
    /// copy. Control frames are skipped rather than answered — nothing here
    /// runs long enough to be pinged.
    fn recv(&mut self) -> Option<Value> {
        loop {
            let mut head = [0u8; 2];
            self.stream.read_exact(&mut head).ok()?;
            let opcode = head[0] & 0x0f;
            let len = match head[1] & 0x7f {
                126 => {
                    let mut b = [0u8; 2];
                    self.stream.read_exact(&mut b).ok()?;
                    u16::from_be_bytes(b) as usize
                }
                127 => {
                    let mut b = [0u8; 8];
                    self.stream.read_exact(&mut b).ok()?;
                    u64::from_be_bytes(b) as usize
                }
                n => n as usize,
            };
            let mut payload = vec![0u8; len];
            self.stream.read_exact(&mut payload).ok()?;
            match opcode {
                0x1 => return serde_json::from_slice(&payload).ok(),
                0x8 => return None, // close
                _ => continue,      // ping/pong/binary: not our business
            }
        }
    }

    /// Everything the server has to say within `secs`.
    ///
    /// Keeps waiting through quiet stretches rather than returning at the
    /// first one — silence is exactly what some of these tests are measuring,
    /// and a `drain` that gave up on it could never observe the heartbeat
    /// snapshot that fires when nothing is happening.
    fn drain(&mut self, secs: u64) -> Vec<Value> {
        let deadline = Instant::now() + Duration::from_secs(secs);
        let mut out = Vec::new();
        while Instant::now() < deadline {
            self.stream
                .get_ref()
                .set_read_timeout(Some(Duration::from_millis(400)))
                .unwrap();
            if let Some(v) = self.recv() {
                out.push(v);
            }
        }
        out
    }
}

#[test]
fn the_socket_pushes_a_whole_picture_before_anything_else() {
    let d = serve();
    let mut ws = Ws::connect(d.port);

    let first = ws.recv().expect("a first frame");
    assert_eq!(
        first["type"], "snapshot",
        "a page that cannot paint from one frame has to fetch, and fetching is what this removed"
    );

    // Everything the dashboard used to poll for, in the one frame.
    for k in [
        "status",
        "sessions",
        "activity",
        "notifications",
        "plan",
        "arch",
        "goals",
    ] {
        assert!(!first[k].is_null(), "snapshot is missing {k}: {first}");
    }
    assert_eq!(first["status"]["agents"][0]["name"], "alice");
    assert_eq!(first["status"]["leases"].as_array().unwrap().len(), 1);
    assert!(
        first["arch"]["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n["name"] == "api"),
        "the architecture rides along, so the graph draws on connect"
    );
}

#[test]
fn events_still_arrive_one_at_a_time_and_are_tagged() {
    let d = serve();
    let mut ws = Ws::connect(d.port);
    let frames = ws.drain(4);

    let events: Vec<&Value> = frames.iter().filter(|f| f["type"] == "event").collect();
    assert!(!events.is_empty(), "the timeline needs its backlog: {frames:?}");
    assert!(
        events.iter().all(|e| e["event"]["kind"].is_string()),
        "the event payload is what it always was, one level down"
    );
}

#[test]
fn a_burst_of_events_is_one_snapshot_not_one_per_event() {
    let d = serve();
    let dir = d._dir.path().to_path_buf();
    let mut ws = Ws::connect(d.port);
    ws.drain(2); // the initial snapshot and backlog

    // Twenty writes as fast as the CLI can make them.
    for i in 0..20 {
        run(
            &dir,
            &[
                "--agent",
                "alice",
                "progress",
                "--percent",
                &(i * 5).to_string(),
                "--note",
                "burst",
            ],
        );
    }

    let frames = ws.drain(4);
    let events = frames.iter().filter(|f| f["type"] == "event").count();
    let snapshots = frames.iter().filter(|f| f["type"] == "snapshot").count();

    assert!(events >= 15, "the burst should be visible: {events} events");
    assert!(
        snapshots < events / 2,
        "the old dashboard fetched /api/status once per event; \
         got {snapshots} snapshots for {events} events"
    );
}

#[test]
fn a_quiet_workspace_still_refreshes_on_the_floor() {
    let d = serve();
    let mut ws = Ws::connect(d.port);
    ws.drain(2);

    // Nobody writes anything. State can still change underneath — a lease
    // ticking toward expiry, an agent going stale — so the picture has to
    // refresh anyway.
    let frames = ws.drain(5);
    assert!(
        frames.iter().any(|f| f["type"] == "snapshot"),
        "a silent event log is not a frozen workspace"
    );
    assert!(
        frames.iter().all(|f| f["type"] == "snapshot"),
        "and nothing else should be chattering: {frames:?}"
    );
}

#[test]
fn the_new_endpoints_answer_over_http_too() {
    let d = serve();
    let base = format!("http://127.0.0.1:{}", d.port);

    // The socket is for the dashboard; these are for everything else.
    for (path, expect) in [
        ("/api/arch", "nodes"),
        ("/api/activity", ""),
        ("/api/notifications", ""),
    ] {
        let body = http_get(&format!("{base}{path}"));
        let v: Value = serde_json::from_str(&body)
            .unwrap_or_else(|e| panic!("{path} returned invalid json: {e}\n{body}"));
        if !expect.is_empty() {
            assert!(!v[expect].is_null(), "{path} is missing {expect}: {v}");
        }
    }
}

/// A one-shot HTTP GET, for the same reason the websocket client is here.
fn http_get(url: &str) -> String {
    let rest = url.strip_prefix("http://").expect("http url");
    let (hostport, path) = rest.split_once('/').expect("path");
    let mut s = TcpStream::connect(hostport).expect("connect");
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    write!(
        s,
        "GET /{path} HTTP/1.1\r\nHost: {hostport}\r\nConnection: close\r\n\r\n"
    )
    .expect("request");
    let mut raw = String::new();
    s.read_to_string(&mut raw).expect("response");
    raw.split_once("\r\n\r\n")
        .map(|(_, body)| body.to_string())
        .unwrap_or(raw)
}
