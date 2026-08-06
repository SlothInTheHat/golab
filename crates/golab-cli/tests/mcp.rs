//! `golab mcp`, end to end, over the real stdio protocol.
//!
//! Spawns the actual binary and speaks JSON-RPC to it, because the claims
//! being tested are about a process: that it registers without being asked,
//! that it stays alive while the model does nothing, that it hands work back
//! when its stdin closes, and that it never writes anything to stdout that is
//! not a protocol frame.

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Output, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::Duration;

use serde_json::{json, Value};

/// Generous: these are cold-start processes on a loaded CI box. What matters
/// is that a hang *fails* rather than blocking the suite forever.
const TIMEOUT: Duration = Duration::from_secs(15);

fn golab() -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_golab"));
    c.env_remove("GOLAB_AGENT");
    c
}

fn run(dir: &Path, args: &[&str]) -> Output {
    golab()
        .current_dir(dir)
        .args(args)
        .output()
        .expect("failed to run golab")
}

fn ok(dir: &Path, args: &[&str]) -> String {
    let out = run(dir, args);
    assert!(
        out.status.success(),
        "golab {args:?} failed ({:?}):\n{}\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn cli_json(dir: &Path, args: &[&str]) -> Value {
    let mut full = vec!["--json"];
    full.extend_from_slice(args);
    serde_json::from_str(&ok(dir, &full)).expect("valid json")
}

/// A minimal MCP client: spawn the server, speak to it, read replies.
struct Mcp {
    child: Child,
    stdin: ChildStdin,
    rx: Receiver<String>,
    next_id: i64,
    /// Every line the server ever wrote, for the stdout-hygiene assertion.
    seen: Vec<String>,
}

impl Mcp {
    fn spawn(dir: &Path, args: &[&str]) -> Mcp {
        let mut cmd = golab();
        cmd.current_dir(dir)
            .arg("mcp")
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // Inherited, so a panic inside the server is visible in the test
            // output instead of vanishing.
            .stderr(Stdio::inherit());
        let mut child = cmd.spawn().expect("failed to spawn golab mcp");
        let stdin = child.stdin.take().expect("stdin");
        let stdout = child.stdout.take().expect("stdout");

        // A reader thread plus recv_timeout: without it a server bug hangs
        // CI instead of failing it.
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                match line {
                    Ok(l) => {
                        if tx.send(l).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        Mcp {
            child,
            stdin,
            rx,
            next_id: 1,
            seen: Vec::new(),
        }
    }

    fn send(&mut self, frame: &Value) {
        let line = serde_json::to_string(frame).unwrap();
        writeln!(self.stdin, "{line}").expect("write to server");
        self.stdin.flush().expect("flush");
    }

    fn recv(&mut self) -> Value {
        match self.rx.recv_timeout(TIMEOUT) {
            Ok(line) => {
                self.seen.push(line.clone());
                serde_json::from_str(&line)
                    .unwrap_or_else(|e| panic!("server wrote a non-JSON line ({e}): {line}"))
            }
            Err(RecvTimeoutError::Timeout) => panic!("server never answered"),
            Err(RecvTimeoutError::Disconnected) => panic!("server closed stdout unexpectedly"),
        }
    }

    /// Send a request and read until the reply with the matching id, skipping
    /// any server-initiated notifications that arrive in between.
    fn req(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        loop {
            let msg = self.recv();
            if msg.get("id").and_then(|v| v.as_i64()) == Some(id) {
                assert!(
                    msg.get("error").is_none(),
                    "{method} returned a protocol error: {msg}"
                );
                return msg["result"].clone();
            }
        }
    }

    fn notify(&mut self, method: &str) {
        self.send(&json!({ "jsonrpc": "2.0", "method": method, "params": {} }));
    }

    fn initialize(&mut self, client: &str) -> Value {
        let result = self.req(
            "initialize",
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": { "name": client, "version": "1.0.0" }
            }),
        );
        self.notify("notifications/initialized");
        result
    }

    /// `tools/call`, returning `structuredContent`.
    fn call(&mut self, tool: &str, args: Value) -> Value {
        let result = self.req("tools/call", json!({ "name": tool, "arguments": args }));
        assert_eq!(
            result["isError"], false,
            "{tool} failed: {}",
            result["content"]
        );
        result["structuredContent"].clone()
    }

    fn call_raw(&mut self, tool: &str, args: Value) -> Value {
        self.req("tools/call", json!({ "name": tool, "arguments": args }))
    }

    /// Close stdin and wait. Returns every line the server wrote.
    fn close(mut self) -> (i32, Vec<String>) {
        drop(self.stdin);
        // Drain whatever is still in flight before reaping.
        while let Ok(line) = self.rx.recv_timeout(Duration::from_millis(500)) {
            self.seen.push(line);
        }
        let status = self.child.wait().expect("server exit");
        (status.code().unwrap_or(-1), self.seen)
    }
}

fn workspace() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(
        dir.path().join("src/pay.ts"),
        "export function charge(x: number) { return x; }\n\
         export function refund(x: number) { return -x; }\n",
    )
    .unwrap();
    ok(dir.path(), &["init"]);
    ok(dir.path(), &["index"]);
    dir
}

#[test]
fn initialize_registers_the_agent_and_opens_a_session() {
    let dir = workspace();
    let d = dir.path();
    let mut mcp = Mcp::spawn(d, &["--as", "alice"]);

    let info = mcp.initialize("claude-code");
    assert_eq!(info["serverInfo"]["name"], "golab");
    assert_eq!(
        info["protocolVersion"], "2025-06-18",
        "a supported version must be echoed back, not replaced"
    );
    assert!(info["capabilities"]["tools"].is_object());

    // The model has called nothing at all, and alice is already a member.
    let swarm = cli_json(d, &["swarm", "list"]);
    let names: Vec<&str> = swarm
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"alice"), "{names:?}");

    let sessions = cli_json(d, &["session", "list", "--live"]);
    let s = &sessions.as_array().unwrap()[0];
    assert_eq!(s["agent"], "alice");
    assert_eq!(s["transport"], "mcp");
    assert_eq!(
        s["tool"], "claude-code",
        "the tool badge comes from the client handshake"
    );

    mcp.close();
}

#[test]
fn two_tools_in_one_workspace_get_distinct_identities() {
    let dir = workspace();
    let d = dir.path();
    ok(d, &["task", "add", "rework charging"]);
    ok(d, &["task", "scope", "T1", "--symbol", "charge"]);
    ok(d, &["task", "add", "rework refunds"]);
    ok(d, &["task", "scope", "T2", "--symbol", "refund"]);

    let mut alice = Mcp::spawn(d, &["--as", "alice", "--tool", "claude-code"]);
    let mut bob = Mcp::spawn(d, &["--as", "bob", "--tool", "cursor"]);
    alice.initialize("claude-code");
    bob.initialize("cursor");

    let a = alice.call("next_task", json!({}));
    let b = bob.call("next_task", json!({}));
    assert_ne!(
        a["task"]["id"], b["task"]["id"],
        "two tools in one repo must never be handed the same work"
    );

    let leases = cli_json(d, &["lease", "list"]);
    let holders: Vec<&str> = leases
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l["agent"].as_str().unwrap())
        .collect();
    assert!(holders.contains(&"alice") && holders.contains(&"bob"), "{holders:?}");

    assert_eq!(cli_json(d, &["session", "list", "--live"]).as_array().unwrap().len(), 2);

    // The workspace default identity file is for humans running the CLI. If
    // an adapter wrote it, the second tool to start would silently believe it
    // was the first — which is exactly the multi-person case this exists for.
    assert!(
        !d.join(".golab").join("agent").exists(),
        "an MCP server must never claim the workspace's default identity"
    );

    alice.close();
    bob.close();
}

#[test]
fn stdout_carries_only_protocol_frames() {
    let dir = workspace();
    let d = dir.path();
    ok(d, &["task", "add", "rework charging"]);
    ok(d, &["task", "scope", "T1", "--symbol", "charge"]);

    let mut mcp = Mcp::spawn(d, &["--as", "alice"]);
    mcp.initialize("claude-code");

    // Drive every write path, since those are the handlers most likely to
    // reach for a println somewhere down the stack.
    mcp.req("tools/list", json!({}));
    mcp.req("resources/list", json!({}));
    mcp.req("resources/read", json!({ "uri": "golab://status" }));
    mcp.req("ping", json!({}));
    mcp.call("next_task", json!({}));
    mcp.call("task_context", json!({}));
    mcp.call("progress", json!({ "percent": 50, "note": "halfway" }));
    mcp.call("claim_symbol", json!({ "symbol": "refund" }));
    mcp.call("note", json!({ "key": "rounding", "value": "half up" }));
    mcp.call("check_edit", json!({ "path": "src/pay.ts" }));
    mcp.call("submit_work", json!({}));

    let (code, lines) = mcp.close();
    assert_eq!(code, 0);
    assert!(!lines.is_empty());
    for line in &lines {
        let v: Value = serde_json::from_str(line)
            .unwrap_or_else(|e| panic!("stray non-JSON on stdout ({e}): {line}"));
        assert_eq!(
            v["jsonrpc"], "2.0",
            "every byte of stdout is a protocol frame: {line}"
        );
    }
}

#[test]
fn eof_releases_leases_and_ends_the_session() {
    let dir = workspace();
    let d = dir.path();
    ok(d, &["task", "add", "rework charging"]);
    ok(d, &["task", "scope", "T1", "--symbol", "charge"]);

    let mut mcp = Mcp::spawn(d, &["--as", "alice"]);
    mcp.initialize("claude-code");
    mcp.call("next_task", json!({}));
    assert_eq!(cli_json(d, &["lease", "list"]).as_array().unwrap().len(), 1);

    let (code, _) = mcp.close();
    assert_eq!(code, 0, "closing stdin is a clean exit, not a crash");
    assert!(
        cli_json(d, &["lease", "list"]).as_array().unwrap().is_empty(),
        "an editor closing should hand work back, not make everyone wait out a TTL"
    );

    let sessions = cli_json(d, &["session", "list"]);
    assert!(sessions.as_array().unwrap()[0]["ended_at"].is_i64());
    assert!(
        cli_json(d, &["swarm", "list"])
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["name"] == "alice"),
        "we end sessions, not people"
    );
}

#[test]
fn keep_leases_survives_a_disconnect() {
    let dir = workspace();
    let d = dir.path();
    ok(d, &["task", "add", "rework charging"]);
    ok(d, &["task", "scope", "T1", "--symbol", "charge"]);

    let mut mcp = Mcp::spawn(d, &["--as", "alice", "--keep-leases"]);
    mcp.initialize("claude-code");
    mcp.call("next_task", json!({}));
    mcp.close();

    assert_eq!(
        cli_json(d, &["lease", "list"]).as_array().unwrap().len(),
        1,
        "a session that will resume keeps what it holds"
    );
}

#[test]
fn heartbeat_keeps_the_agent_online_with_no_tool_calls() {
    let dir = workspace();
    let d = dir.path();
    ok(d, &["task", "add", "rework charging"]);
    ok(d, &["task", "scope", "T1", "--symbol", "charge"]);

    let mut mcp = Mcp::spawn(d, &["--as", "alice", "--heartbeat-secs", "1"]);
    mcp.initialize("claude-code");
    let claimed = mcp.call("next_task", json!({}));
    assert_eq!(claimed["claimed"], true);
    let before = cli_json(d, &["lease", "list"])[0]["expires_at"]
        .as_i64()
        .unwrap();

    // The model does nothing at all for several ticks. This is the whole
    // adapter claim: liveness is not the model's job.
    thread::sleep(Duration::from_millis(3_500));

    let agents = cli_json(d, &["swarm", "list"]);
    let alice = agents
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["name"] == "alice")
        .unwrap();
    assert_eq!(alice["online"], true, "presence must not depend on the model");

    let after = cli_json(d, &["lease", "list"])[0]["expires_at"]
        .as_i64()
        .unwrap();
    assert!(
        after > before,
        "the lease should have been renewed underneath it ({before} -> {after})"
    );

    mcp.close();
}

#[test]
fn a_denied_edit_is_an_answer_rather_than_an_error() {
    let dir = workspace();
    let d = dir.path();
    ok(d, &["--agent", "carol", "swarm", "join", "carol"]);
    ok(d, &["--agent", "carol", "lease", "acquire", "charge"]);

    let mut mcp = Mcp::spawn(d, &["--as", "alice"]);
    mcp.initialize("claude-code");

    let raw = mcp.call_raw("check_edit", json!({ "path": "src/pay.ts" }));
    assert_eq!(
        raw["isError"], false,
        "a refusal is a normal answer the model must branch on, not a fault"
    );
    let report = &raw["structuredContent"];
    assert_eq!(report["verdict"], "denied");
    assert_eq!(report["conflicts"][0]["holder"], "carol");
    assert!(
        raw["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("DO NOT EDIT"),
        "the text half has to be unambiguous too: {}",
        raw["content"][0]["text"]
    );

    mcp.close();
}

#[test]
fn a_stuck_agent_can_ask_for_the_symbol_and_be_handed_it() {
    let dir = workspace();
    let d = dir.path();
    ok(d, &["--agent", "carol", "swarm", "join", "carol"]);
    ok(d, &["--agent", "carol", "lease", "acquire", "charge"]);

    let mut mcp = Mcp::spawn(d, &["--as", "alice"]);
    mcp.initialize("claude-code");

    // The runtime knows who holds it, so the asker never has to work out the
    // addressee — that is what makes this one call instead of three.
    let opened = mcp.call("ask", json!({
        "kind": "lease-transfer",
        "symbol": "charge",
        "reason": "production hotfix"
    }));
    assert_eq!(opened["to"], "carol");
    let request_id = opened["id"].as_str().unwrap().to_string();

    ok(d, &["--agent", "carol", "request", "accept", &request_id]);

    let owns = mcp.call("who_owns", json!({ "symbol": "charge" }));
    assert!(
        owns["yours"].is_object(),
        "accepting a lease-transfer performs the handover: {owns}"
    );

    mcp.close();
}

#[test]
fn an_unknown_tool_is_a_protocol_error_but_a_failing_tool_is_not() {
    let dir = workspace();
    let d = dir.path();
    let mut mcp = Mcp::spawn(d, &["--as", "alice"]);
    mcp.initialize("claude-code");

    // Calling something that does not exist is the model getting the API
    // wrong; that belongs on the error channel.
    mcp.send(&json!({
        "jsonrpc": "2.0", "id": 900, "method": "tools/call",
        "params": { "name": "no_such_tool", "arguments": {} }
    }));
    let reply = loop {
        let m = mcp.recv();
        if m.get("id").and_then(|v| v.as_i64()) == Some(900) {
            break m;
        }
    };
    assert_eq!(reply["error"]["code"], -32601);

    // A tool that ran and failed belongs in the result, so the model can see
    // it and recover.
    let failed = mcp.call_raw("release_symbol", json!({ "symbol": "charge" }));
    assert_eq!(failed["isError"], true);
    assert!(failed["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("no lease"));

    mcp.close();
}

#[test]
fn the_tool_surface_is_documented_and_schema_bearing() {
    let dir = workspace();
    let d = dir.path();
    let mut mcp = Mcp::spawn(d, &["--as", "alice"]);
    mcp.initialize("claude-code");

    let listed = mcp.req("tools/list", json!({}));
    let tools = listed["tools"].as_array().unwrap();
    assert!(tools.len() >= 15, "expected the full surface, got {}", tools.len());
    for t in tools {
        let name = t["name"].as_str().unwrap();
        assert!(!name.starts_with("golab_"), "clients namespace these already: {name}");
        assert!(
            t["description"].as_str().unwrap().len() > 40,
            "{name}'s description is the API the model programs against"
        );
        assert_eq!(t["inputSchema"]["type"], "object", "{name}");
    }

    let resources = mcp.req("resources/list", json!({}))["resources"]
        .as_array()
        .unwrap()
        .len();
    assert_eq!(resources, 5);

    mcp.close();
}

#[test]
fn notices_ride_along_on_a_tool_result_the_model_was_making_anyway() {
    let dir = workspace();
    let d = dir.path();
    ok(d, &["--agent", "carol", "swarm", "join", "carol"]);
    ok(d, &["--agent", "carol", "lease", "acquire", "charge"]);

    let mut mcp = Mcp::spawn(d, &["--as", "alice"]);
    mcp.initialize("claude-code");
    // Drain anything from before this point so the assertion is about what
    // happens next, not about startup.
    mcp.call("whoami", json!({}));

    // Carol wants something from alice. Nothing pushes this at the model —
    // MCP has no mechanism for it — so it has to arrive on the next result.
    ok(
        d,
        &[
            "--agent", "carol", "request", "open", "--to", "alice", "--kind", "question",
            "--subject", "can you take the refund path?",
        ],
    );

    let raw = mcp.call_raw("whoami", json!({}));
    let notices = &raw["structuredContent"]["notices"];
    assert_eq!(
        notices["inbox"][0]["from"], "carol",
        "the request has to reach the model without it asking: {notices}"
    );

    let text = raw["content"][0]["text"].as_str().unwrap();
    assert!(
        text.contains("[golab]") && text.contains("carol asks"),
        "and in the text half, which is the part clients reliably show: {text}"
    );
    assert!(
        text.contains("respond request="),
        "telling the model without telling it how to reply is half a channel: {text}"
    );

    // Answering it makes the notice go away rather than repeat forever.
    let id = notices["inbox"][0]["id"].as_str().unwrap().to_string();
    mcp.call("respond", json!({ "request": id, "action": "decline", "reason": "busy" }));
    let after = mcp.call_raw("whoami", json!({}));
    let inbox = &after["structuredContent"]["notices"]["inbox"];
    assert!(
        inbox.is_null() || inbox.as_array().map(|a| a.is_empty()).unwrap_or(true),
        "an answered request must stop being reported: {after}"
    );
}

#[test]
fn a_quiet_workspace_adds_nothing_to_a_tool_result() {
    let dir = workspace();
    let d = dir.path();
    let mut mcp = Mcp::spawn(d, &["--as", "alice"]);
    mcp.initialize("claude-code");

    let raw = mcp.call_raw("whoami", json!({}));
    assert!(
        raw["structuredContent"]["notices"].is_null(),
        "an empty notices block on every call would be pure token cost"
    );
    assert!(!raw["content"][0]["text"].as_str().unwrap().contains("[golab]"));

    mcp.close();
}

#[test]
fn wait_for_returns_the_answer_rather_than_making_the_model_poll() {
    let dir = workspace();
    let d = dir.path();
    ok(d, &["--agent", "carol", "swarm", "join", "carol"]);
    ok(d, &["--agent", "carol", "lease", "acquire", "charge"]);

    let mut mcp = Mcp::spawn(d, &["--as", "alice"]);
    mcp.initialize("claude-code");
    let opened = mcp.call("ask", json!({
        "kind": "lease-transfer", "symbol": "charge", "reason": "hotfix"
    }));
    let id = opened["id"].as_str().unwrap().to_string();

    // Carol answers while alice is blocked in the call.
    let answer_dir = d.to_path_buf();
    let answer_id = id.clone();
    let answering = thread::spawn(move || {
        thread::sleep(Duration::from_millis(600));
        ok(&answer_dir, &["--agent", "carol", "request", "accept", &answer_id]);
    });

    let result = mcp.call("wait_for", json!({ "request": id, "max_secs": 10 }));
    answering.join().unwrap();
    assert_eq!(
        result["state"], "fulfilled",
        "accepting a lease-transfer resolves it outright: {result}"
    );

    let owns = mcp.call("who_owns", json!({ "symbol": "charge" }));
    assert!(owns["yours"].is_object(), "and the symbol actually moved: {owns}");

    mcp.close();
}

#[test]
fn wait_for_gives_up_rather_than_hanging_the_client() {
    let dir = workspace();
    let d = dir.path();
    ok(d, &["--agent", "carol", "swarm", "join", "carol"]);
    ok(d, &["--agent", "carol", "lease", "acquire", "charge"]);

    let mut mcp = Mcp::spawn(d, &["--as", "alice"]);
    mcp.initialize("claude-code");
    let opened = mcp.call("ask", json!({
        "kind": "lease-transfer", "symbol": "charge", "reason": "hotfix"
    }));

    // Nobody answers. A tool call that outlives the client's own timeout looks
    // like a hung server, which is worse than saying "still open".
    let result = mcp.call("wait_for", json!({
        "request": opened["id"].as_str().unwrap(), "max_secs": 1
    }));
    assert_eq!(result["state"], "open");

    mcp.close();
}

#[test]
fn an_idle_agent_is_oriented_without_reading_the_repository() {
    let dir = workspace();
    let d = dir.path();
    ok(d, &["goal", "add", "make charging refundable"]);
    ok(
        d,
        &["goal", "decompose", "G1", "--task", "wire refunds", "--symbol", "refund"],
    );

    let mut mcp = Mcp::spawn(d, &["--as", "alice"]);
    mcp.initialize("claude-code");

    let ctx = mcp.call("task_context", json!({}));
    assert_eq!(
        ctx["context"], "agent",
        "the payload differs by state, so it has to say which shape it is"
    );
    assert!(ctx["task"].is_null(), "nothing claimed yet");
    assert_eq!(ctx["open_goals"][0]["id"], "G1");
    assert_eq!(ctx["startable"][0]["id"], "T1");

    mcp.call("next_task", json!({}));
    let ctx = mcp.call("task_context", json!({}));
    assert_eq!(ctx["context"], "task");
    assert_eq!(ctx["task"]["id"], "T1");
    assert_eq!(ctx["scope"][0]["symbol"]["name"], "refund");
    assert_eq!(
        ctx["scope"][0]["lease"]["agent"], "alice",
        "and it says the scope is already yours"
    );

    mcp.close();
}
