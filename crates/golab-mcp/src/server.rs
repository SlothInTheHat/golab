//! Session lifecycle and message dispatch.
//!
//! Three threads, three `Store` connections:
//!
//! | thread    | owns    | does                                              |
//! |-----------|---------|---------------------------------------------------|
//! | main      | Store A | read stdin, dispatch, hand frames to the writer    |
//! | heartbeat | Store B | every tick: heartbeat, renew leases, sweep         |
//! | writer    | —       | sole owner of stdout; one frame per line, flushed  |
//!
//! Two connections in one process is the same situation as two processes,
//! which WAL plus a busy timeout plus `IMMEDIATE` transactions already handle —
//! `crates/golab-cli/tests/cli.rs` proves that path with eight racing
//! processes. What it buys is that the heartbeat cannot block behind a long
//! tool call, which a shared `Mutex<Store>` (the daemon's pattern, correct
//! there) would not give us.
//!
//! The writer thread is not optional either: without a single owner of stdout,
//! the heartbeat thread's notifications and the main thread's responses would
//! interleave mid-frame and corrupt the stream.

use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::mpsc::{self, Sender};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::thread;
use std::time::Duration;

use anyhow::{anyhow, Result};
use golab_core::identity;
use golab_core::session::{transport, NewSession};
use golab_core::Store;
use serde_json::{json, Value};

use crate::jsonrpc::{self, Incoming};
use crate::{resources, tools, McpConfig, SUPPORTED_PROTOCOL_VERSIONS};

/// Everything a tool handler needs. One per session; the main thread owns it.
pub struct Session {
    pub store: Store,
    pub root: PathBuf,
    pub agent: String,
    pub tool: String,
    pub session_id: Option<String>,
    pub cfg: McpConfig,
    /// How far through the event log this agent has already been told about.
    /// Advanced only when a notice actually reaches the client, so nothing is
    /// silently skipped.
    pub event_cursor: i64,
}

impl Session {
    /// The repo a bare symbol query belongs to. Path-taking operations route
    /// per repo via `golab_core::workspace`; this is the fallback for the
    /// operations that take a symbol reference instead.
    pub fn repo_id(&self) -> &str {
        golab_core::ids::DEFAULT_REPO_ID
    }
}

/// What the model is told about the workspace it just joined. Clients differ
/// on whether they surface this, so nothing depends on it — every rule it
/// states is also enforced by the tools themselves.
const INSTRUCTIONS: &str = "\
This repository is coordinated by golab: other people and other AI agents are \
working in it at the same time.

Before editing a file, call `check_edit`. If it comes back `denied`, somebody \
else holds that code — do not edit it. Use `ask` to request a handover, or \
narrow your edit to a symbol they do not hold; the report tells you which.

Call `next_task` to be given work, `task_context` to understand it without \
re-reading the repository, `progress` as you go, and `submit_work` when done. \
`inbox` and `respond` are how other agents talk to you; answer them.

You are registered and heartbeated automatically — never run lease commands by \
hand.";

pub fn run(cfg: McpConfig, input: impl BufRead, output: impl Write + Send + 'static) -> Result<()> {
    let store = Store::open(&golab_core::db_path(&cfg.root))?;
    let (tx, rx) = mpsc::channel::<String>();

    // One owner of stdout, for the whole life of the process.
    let writer = thread::spawn(move || {
        let mut out = output;
        for frame in rx {
            if out.write_all(frame.as_bytes()).is_err() || out.write_all(b"\n").is_err() {
                break;
            }
            if out.flush().is_err() {
                break;
            }
        }
    });

    let mut session = Session {
        // Start from the present: a session should not be handed a backlog of
        // everything that happened while nobody was connected.
        event_cursor: store.last_event_id().unwrap_or(0),
        store,
        root: cfg.root.clone(),
        agent: String::new(),
        tool: cfg.tool.clone().unwrap_or_else(|| "mcp".to_string()),
        session_id: None,
        cfg: cfg.clone(),
    };

    let stop = Arc::new(AtomicBool::new(false));
    let mut heartbeat: Option<thread::JoinHandle<()>> = None;

    for line in input.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break, // stdin died; treat exactly as EOF
        };
        let msg = match jsonrpc::parse(&line) {
            Ok(None) => continue,
            Ok(Some(m)) => m,
            Err(e) => {
                let _ = tx.send(jsonrpc::err(Value::Null, jsonrpc::PARSE_ERROR, &e));
                continue;
            }
        };

        // `notifications/initialized` is the client saying it is ready; that
        // is when background work may start writing to the stream.
        if msg.method == "notifications/initialized" && heartbeat.is_none() {
            heartbeat = Some(spawn_heartbeat(&session, &cfg, tx.clone(), stop.clone()));
        }

        if let Some(frame) = dispatch(&mut session, &msg) {
            let _ = tx.send(frame);
        }
    }

    // stdin closed: the tool is gone.
    stop.store(true, Ordering::Relaxed);
    if let Some(sid) = session.session_id.clone() {
        session
            .store
            .end_session(&sid, !session.cfg.keep_leases)
            .ok();
    }
    drop(tx);
    let _ = writer.join();
    if let Some(h) = heartbeat {
        let _ = h.join();
    }
    Ok(())
}

/// `None` for a notification, which must never be answered.
fn dispatch(session: &mut Session, msg: &Incoming) -> Option<String> {
    if msg.is_notification() {
        // `notifications/cancelled` and friends: accept and carry on. An
        // unknown notification is not an error either, per the spec.
        return None;
    }
    let id = msg.id.clone().unwrap_or(Value::Null);

    let result = match msg.method.as_str() {
        "initialize" => initialize(session, &msg.params),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(tools::list()),
        "tools/call" => return Some(call_tool(session, id, &msg.params)),
        "resources/list" => Ok(resources::list()),
        "resources/read" => resources::read(session, &msg.params),
        other => {
            return Some(jsonrpc::err(
                id,
                jsonrpc::METHOD_NOT_FOUND,
                &format!("unknown method: {other}"),
            ))
        }
    };

    Some(match result {
        Ok(v) => jsonrpc::ok(id, v),
        Err(e) => jsonrpc::err(id, jsonrpc::INVALID_PARAMS, &format!("{e:#}")),
    })
}

/// Register and attach. This is the whole "adapter" claim: by the time the
/// client's first tool call arrives, the agent already exists, already has a
/// session, and is already being heartbeated — none of which the model asked
/// for or could have forgotten to do.
fn initialize(session: &mut Session, params: &Value) -> Result<Value> {
    let client_tool = params
        .get("clientInfo")
        .and_then(|c| c.get("name"))
        .and_then(|n| n.as_str());
    let tool = session
        .cfg
        .tool
        .clone()
        .or_else(|| client_tool.map(identity::slug))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "mcp".to_string());

    let agent = identity::derive_agent(&tool, session.cfg.agent.as_deref());
    // `kind` is documented free-form with no behaviour attached, so putting
    // the tool there costs nothing and immediately gives every existing view —
    // `swarm list`, the dashboard — a real tool badge.
    session.store.register_agent(&agent, &tool)?;

    let opened = session.store.open_session(&NewSession {
        client_key: params
            .get("clientInfo")
            .and_then(|c| c.get("version"))
            .and_then(|v| v.as_str())
            .map(|v| format!("{tool}@{v}")),
        pid: Some(std::process::id() as i64),
        ..NewSession::new(
            &agent,
            &tool,
            transport::MCP,
            &session.root.to_string_lossy(),
        )
    })?;

    session.agent = agent.clone();
    session.tool = tool;
    session.session_id = Some(opened.id);

    let requested = params.get("protocolVersion").and_then(|v| v.as_str());
    let version = match requested {
        Some(v) if SUPPORTED_PROTOCOL_VERSIONS.contains(&v) => v,
        // Not one we know: offer our newest and let the client decide whether
        // it can live with it.
        _ => SUPPORTED_PROTOCOL_VERSIONS[0],
    };

    Ok(json!({
        "protocolVersion": version,
        "capabilities": {
            "tools": { "listChanged": false },
            "resources": { "listChanged": true, "subscribe": false },
        },
        "serverInfo": { "name": "golab", "version": env!("CARGO_PKG_VERSION") },
        "instructions": INSTRUCTIONS,
    }))
}

fn call_tool(session: &mut Session, id: Value, params: &Value) -> String {
    let Some(name) = params.get("name").and_then(|n| n.as_str()) else {
        return jsonrpc::err(id, jsonrpc::INVALID_PARAMS, "tools/call needs a name");
    };
    let args = params.get("arguments").cloned().unwrap_or(json!({}));

    let Some(tool) = tools::find(name) else {
        // An unknown tool is a protocol fault, not a tool failure: the model
        // called something that does not exist.
        return jsonrpc::err(
            id,
            jsonrpc::METHOD_NOT_FOUND,
            &format!("unknown tool: {name}"),
        );
    };

    match (tool.handler)(session, &args) {
        Ok(outcome) => {
            // Every result carries what happened elsewhere. This is the only
            // channel with a guaranteed path into the model's context — see
            // `notices.rs` for why server-initiated notifications are not.
            let notices = collect_notices(session);
            jsonrpc::ok(id, outcome.into_result(notices))
        }
        // A tool that *failed* belongs in the result, not in a JSON-RPC error:
        // the model has to be able to see it and recover. Only protocol faults
        // above get the error channel.
        Err(e) => jsonrpc::ok(
            id,
            json!({
                "content": [{ "type": "text", "text": format!("{e:#}") }],
                "isError": true,
            }),
        ),
    }
}

/// Never let a notice failure take down a tool call that otherwise worked.
fn collect_notices(session: &mut Session) -> Option<crate::notices::Notices> {
    if session.agent.is_empty() {
        return None;
    }
    let mut cursor = session.event_cursor;
    let notices = crate::notices::collect(&session.store, &session.agent, &mut cursor).ok()?;
    session.event_cursor = cursor;
    if notices.is_empty() {
        None
    } else {
        Some(notices)
    }
}

fn spawn_heartbeat(
    session: &Session,
    cfg: &McpConfig,
    tx: Sender<String>,
    stop: Arc<AtomicBool>,
) -> thread::JoinHandle<()> {
    let root = cfg.root.clone();
    let agent = session.agent.clone();
    let session_id = session.session_id.clone();
    let every = Duration::from_secs(cfg.heartbeat_secs.max(1));

    thread::spawn(move || {
        // Its own connection, so a long tool call on the main thread cannot
        // starve presence.
        let Ok(mut store) = Store::open(&golab_core::db_path(&root)) else {
            return;
        };
        let mut cursor = store.last_event_id().unwrap_or(0);

        while !stop.load(Ordering::Relaxed) {
            // Wake often, work rarely: a sleeping thread would delay shutdown
            // by up to a full interval on every exit.
            let mut waited = Duration::ZERO;
            while waited < every && !stop.load(Ordering::Relaxed) {
                thread::sleep(Duration::from_millis(200));
                waited += Duration::from_millis(200);
            }
            if stop.load(Ordering::Relaxed) {
                break;
            }

            if !agent.is_empty() {
                store.heartbeat(&agent, None).ok();
            }
            if let Some(sid) = &session_id {
                store.heartbeat_session(sid).ok();
            }
            store.sweep().ok();

            // Somebody has to notice things the model did not cause. Polling
            // the table here is the same argument the daemon makes for its own
            // pump: other processes write events too, so in-process
            // broadcasting would miss exactly the events that matter most.
            if let Ok(events) = store.events_since(cursor, 200) {
                if let Some(last) = events.last() {
                    cursor = last.id;
                    if events.iter().any(|e| e.agent.as_deref() != Some(&agent)) {
                        // Best effort by construction: some clients re-read
                        // resources on this, most ignore it. The channel that
                        // is actually guaranteed to reach the model is the
                        // notices block on every tool result.
                        let _ = tx.send(jsonrpc::notify(
                            "notifications/resources/list_changed",
                            json!({}),
                        ));
                    }
                }
            }
        }
    })
}

/// What a tool handler produces.
pub struct ToolOutcome {
    /// Compact enough for a model's context on every call.
    pub text: String,
    pub data: Value,
}

impl ToolOutcome {
    pub fn new(text: impl Into<String>, data: Value) -> ToolOutcome {
        ToolOutcome {
            text: text.into(),
            data,
        }
    }

    fn into_result(self, notices: Option<crate::notices::Notices>) -> Value {
        let mut data = self.data;
        let mut text = self.text;

        if let Some(n) = notices {
            // The text half as well as the structured half: clients reliably
            // show `content`, and are inconsistent about `structuredContent`.
            let line = n.summary();
            if !line.is_empty() {
                text.push_str("\n\n");
                text.push_str(&line);
            }
            if let Some(obj) = data.as_object_mut() {
                obj.insert(
                    "notices".to_string(),
                    serde_json::to_value(&n).unwrap_or(Value::Null),
                );
            } else {
                data = json!({ "result": data, "notices": n });
            }
        }

        json!({
            "content": [{ "type": "text", "text": text }],
            "structuredContent": data,
            "isError": false,
        })
    }
}

/// Argument helpers. Missing is not an error unless the handler says so —
/// models omit optional fields constantly.
pub fn arg_str<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(|v| v.as_str()).filter(|s| !s.is_empty())
}

pub fn arg_i64(args: &Value, key: &str) -> Option<i64> {
    args.get(key).and_then(|v| v.as_i64())
}

pub fn arg_bool(args: &Value, key: &str, default: bool) -> bool {
    args.get(key).and_then(|v| v.as_bool()).unwrap_or(default)
}

pub fn require_str<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    arg_str(args, key).ok_or_else(|| anyhow!("missing required argument: {key}"))
}
