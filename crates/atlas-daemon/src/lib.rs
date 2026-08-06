//! HTTP API, live event stream and dashboard.
//!
//! The daemon is optional: coordination correctness lives in the database, so
//! the CLI works with nothing running. What the daemon adds is *observability*
//! — a websocket feed of the event log and a dashboard humans can watch while
//! a swarm of agents works.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use atlas_core::lease::AcquireOptions;
use atlas_core::protocol::{Direction, NewRequest};
use atlas_core::{workspace, Store};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::broadcast;

pub mod watcher;

const DASHBOARD: &str = include_str!("dashboard.html");

/// How often the poller sweeps the event log and retires expired leases.
const TICK_MS: u64 = 300;

/// Push a snapshot at least this often even when the event log is quiet.
///
/// Some state changes without anybody writing an event: a lease ticking toward
/// expiry, an agent going stale. A floor means the picture cannot sit wrong for
/// longer than this, without turning the socket into a polling loop.
const SNAPSHOT_FLOOR_MS: i64 = 3_000;

/// How deep the architecture graph is pushed by default.
///
/// Services only. It is the picture a person can hold in their head, and the
/// dashboard asks `/api/arch` directly when someone drills in.
const SNAPSHOT_ARCH_DEPTH: usize = 1;

#[derive(Clone)]
struct AppState {
    root: PathBuf,
    store: Arc<Mutex<Store>>,
    events: broadcast::Sender<String>,
    /// The most recent snapshot, so a browser that has just connected gets the
    /// whole picture immediately instead of waiting for the next tick.
    latest: Arc<Mutex<Option<String>>>,
}

/// What travels over `/ws`.
///
/// Tagged, because there are now two kinds of frame and a client has to be able
/// to tell them apart. The `event` payload is byte-for-byte what the socket
/// used to send bare, so the only migration is reading `.event` instead of the
/// whole message.
#[derive(serde::Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum WsFrame {
    /// One line of the narrative, as it happens.
    Event { event: atlas_core::model::Event },
    /// The whole picture, coalesced to at most one per tick.
    ///
    /// Boxed: a snapshot is orders of magnitude larger than an event, and
    /// events are the frame that gets constructed in bulk.
    Snapshot(Box<Snapshot>),
}

#[derive(serde::Serialize)]
struct Snapshot {
    status: atlas_core::work::StatusSummary,
    sessions: Vec<atlas_core::session::SessionView>,
    activity: Vec<atlas_core::activity::ActivityView>,
    notifications: Vec<atlas_core::notice::Notice>,
    plan: Option<Box<atlas_core::schedule::Plan>>,
    arch: Option<Box<atlas_core::arch::ArchGraph>>,
    goals: Vec<atlas_core::context::GoalWithProgress>,
    at: i64,
}

/// Serve until the process is interrupted.
///
/// With `watch`, a background thread keeps the index current as files change,
/// so the graph the API serves is never a stale snapshot.
pub async fn serve(root: PathBuf, addr: &str, watch: bool) -> Result<()> {
    let store = Store::open(&atlas_core::db_path(&root))?;
    let (tx, _) = broadcast::channel(1024);
    let state = AppState {
        root,
        store: Arc::new(Mutex::new(store)),
        events: tx,
        latest: Arc::new(Mutex::new(None)),
    };

    tokio::spawn(pump(state.clone()));

    if watch {
        // Its own thread and its own connection: indexing must not block the
        // event pump or an in-flight request.
        let watch_root = state.root.clone();
        std::thread::spawn(move || {
            if let Err(e) = watcher::watch(&watch_root, watcher::Stop::new(), |_, _| {}) {
                eprintln!("atlas: watcher stopped: {e:#}");
            }
        });
    }

    let app = Router::new()
        .route("/", get(dashboard))
        .route("/ws", get(ws_handler))
        .route("/api/status", get(api_status))
        .route("/api/symbols", get(api_symbols))
        .route("/api/graph/{id}", get(api_graph))
        .route("/api/leases", get(api_leases).post(api_acquire))
        .route("/api/leases/{id}", axum::routing::delete(api_release))
        .route("/api/leases/{id}/renew", post(api_renew))
        .route("/api/leases/{id}/transfer", post(api_transfer))
        .route("/api/events", get(api_events))
        .route("/api/tasks", get(api_tasks))
        .route("/api/schedule", get(api_schedule))
        .route("/api/schedule/claim", post(api_claim))
        .route("/api/agents", get(api_agents))
        .route("/api/agents/{name}/pause", post(api_pause))
        .route("/api/tasks/{id}/priority", post(api_priority))
        .route("/api/tasks/{id}/assign", post(api_assign))
        .route("/api/tasks/{id}/state", post(api_task_state))
        .route("/api/throughput", get(api_throughput))
        .route("/api/requests", get(api_requests).post(api_open_request))
        .route("/api/requests/{id}", get(api_request))
        .route("/api/requests/{id}/accept", post(api_accept))
        .route("/api/requests/{id}/decline", post(api_decline))
        .route("/api/requests/{id}/fulfill", post(api_fulfill))
        .route("/api/progress", get(api_progress).post(api_post_progress))
        .route("/api/endpoints", get(api_endpoints))
        .route("/api/tables", get(api_tables))
        .route("/api/services", get(api_services))
        .route("/api/owners", get(api_owners))
        .route("/api/check", get(api_check))
        .route("/api/scan", post(api_scan))
        .route("/api/goals", get(api_goals).post(api_add_goal))
        .route("/api/goals/{id}", get(api_goal))
        .route("/api/goals/{id}/progress", get(api_goal_progress))
        .route("/api/goals/{id}/decompose", post(api_goal_decompose))
        .route("/api/goals/{id}/suggest", get(api_goal_suggest))
        .route("/api/goals/{id}/plan", get(api_goal_plan))
        .route("/api/tasks/{id}/review/submit", post(api_review_submit))
        .route("/api/tasks/{id}/review/approve", post(api_review_approve))
        .route("/api/tasks/{id}/review/reject", post(api_review_reject))
        .route("/api/observe", get(api_observe))
        .route("/api/continue", post(api_continue))
        .route("/api/repos", get(api_repos))
        .route("/api/guard", get(api_guard))
        .route("/api/context", get(api_context))
        .route("/api/sessions", get(api_sessions))
        .route(
            "/api/sessions/{id}",
            axum::routing::delete(api_end_session),
        )
        .route("/api/memory", get(api_memory).post(api_memory_set))
        .route(
            "/api/memory/{key}",
            get(api_memory_get).delete(api_memory_delete),
        )
        .route("/api/agents/{name}/capability", post(api_capability))
        .route(
            "/api/agents/{name}",
            axum::routing::delete(api_agent_leave),
        )
        .route("/api/tasks/{id}/scope", post(api_task_scope))
        .route("/api/arch", get(api_arch))
        .route("/api/arch/{node}", get(api_arch_node))
        .route("/api/activity", get(api_activity))
        .route("/api/notifications", get(api_notifications))
        .layer(tower_http::cors::CorsLayer::permissive())
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

/// Tails the event log, fans it out, and pushes a coalesced state snapshot.
///
/// # Why the daemon still polls, and the browser no longer does
///
/// Polling the *table* rather than broadcasting in-process is deliberate and
/// load-bearing: CLI invocations in other processes write events too, and they
/// must show up on the dashboard identically. That has not changed.
///
/// What changed is who does it. The dashboard used to run six `setInterval`s
/// *and* refetch `/api/status` once per received event — so a burst of two
/// hundred events fired two hundred requests, precisely when the server was
/// busiest. Now one snapshot is built here per tick, no matter how many events
/// landed in it, and every connected browser gets it pushed. A client that
/// polls nothing cannot stampede.
async fn pump(state: AppState) {
    let mut cursor = {
        let store = state.store.lock().expect("store poisoned");
        store.last_event_id().unwrap_or(0)
    };
    let mut last_snapshot: i64 = 0;
    let mut ticker = tokio::time::interval(std::time::Duration::from_millis(TICK_MS));
    loop {
        ticker.tick().await;
        let batch = {
            let mut store = state.store.lock().expect("store poisoned");
            // Someone has to notice that TTLs ran out; make it the daemon.
            store.sweep().ok();
            store.events_since(cursor, 200).unwrap_or_default()
        };
        let moved = !batch.is_empty();
        for e in batch {
            cursor = e.id;
            if let Ok(text) = serde_json::to_string(&WsFrame::Event { event: e }) {
                let _ = state.events.send(text);
            }
        }

        let now = atlas_core::now_ms();
        if moved || now - last_snapshot >= SNAPSHOT_FLOOR_MS {
            last_snapshot = now;
            if let Some(text) = build_snapshot(&state) {
                *state.latest.lock().expect("latest poisoned") = Some(text.clone());
                let _ = state.events.send(text);
            }
        }
    }
}

/// One frame carrying everything the dashboard used to fetch separately.
fn build_snapshot(state: &AppState) -> Option<String> {
    let store = state.store.lock().expect("store poisoned");
    let frame = WsFrame::Snapshot(Box::new(Snapshot {
        status: store.status(30).ok()?,
        sessions: store.sessions(false).unwrap_or_default(),
        activity: store.live_activity().unwrap_or_default(),
        notifications: store.notifications(None, 25).unwrap_or_default(),
        plan: store.plan().ok().map(Box::new),
        arch: store
            .architecture(None, SNAPSHOT_ARCH_DEPTH)
            .ok()
            .map(Box::new),
        goals: store.goals_with_progress().unwrap_or_default(),
        at: atlas_core::now_ms(),
    }));
    serde_json::to_string(&frame).ok()
}

async fn dashboard() -> Html<&'static str> {
    Html(DASHBOARD)
}

async fn ws_handler(ws: WebSocketUpgrade, State(state): State<AppState>) -> Response {
    ws.on_upgrade(move |socket| ws_stream(socket, state))
}

async fn ws_stream(mut socket: WebSocket, state: AppState) {
    // Subscribe *before* sending the backlog, so nothing that happens while we
    // are catching up falls down the gap between the two.
    let mut rx = state.events.subscribe();

    // A snapshot first: the page has to be able to paint from one frame, or it
    // would need to fetch — which is the thing being removed.
    let first = {
        let cached = state.latest.lock().expect("latest poisoned").clone();
        match cached {
            Some(text) => Some(text),
            // Nothing cached yet means the pump has not ticked. Build it here
            // rather than leave the first visitor staring at "loading…".
            None => build_snapshot(&state),
        }
    };
    if let Some(text) = first {
        if socket.send(Message::Text(text.into())).await.is_err() {
            return;
        }
    }

    // Then recent history, so the timeline is not empty on a fresh page.
    let backlog = {
        let store = state.store.lock().expect("store poisoned");
        store.recent_events(50).unwrap_or_default()
    };
    for e in backlog {
        if let Ok(text) = serde_json::to_string(&WsFrame::Event { event: e }) {
            if socket.send(Message::Text(text.into())).await.is_err() {
                return;
            }
        }
    }

    loop {
        match rx.recv().await {
            Ok(text) => {
                if socket.send(Message::Text(text.into())).await.is_err() {
                    return;
                }
            }
            Err(broadcast::error::RecvError::Lagged(_)) => continue,
            Err(broadcast::error::RecvError::Closed) => return,
        }
    }
}

// ----------------------------------------------------------------- handlers

fn fail(e: anyhow::Error) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({ "error": e.to_string() })),
    )
        .into_response()
}

async fn api_status(State(state): State<AppState>) -> Response {
    let store = state.store.lock().expect("store poisoned");
    match store.status(30) {
        Ok(s) => Json(s).into_response(),
        Err(e) => fail(e),
    }
}

#[derive(Deserialize)]
struct SymbolQuery {
    pattern: Option<String>,
    path: Option<String>,
    kind: Option<String>,
    #[serde(default = "default_limit")]
    limit: usize,
}

fn default_limit() -> usize {
    200
}

async fn api_symbols(State(state): State<AppState>, Query(q): Query<SymbolQuery>) -> Response {
    let store = state.store.lock().expect("store poisoned");
    let kind = q
        .kind
        .as_deref()
        .and_then(atlas_core::model::SymbolKind::parse);
    match store.list_symbols(q.path.as_deref(), kind, q.pattern.as_deref(), q.limit) {
        Ok(v) => Json(v).into_response(),
        Err(e) => fail(e),
    }
}

#[derive(Deserialize)]
struct DepthQuery {
    #[serde(default = "default_depth")]
    depth: usize,
}

fn default_depth() -> usize {
    2
}

async fn api_graph(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<DepthQuery>,
) -> Response {
    let store = state.store.lock().expect("store poisoned");
    let result = store.resolve(&id).and_then(|sym| {
        let neighbors = store.neighbors(&sym.id)?;
        let impact = store.impact(&sym.id, q.depth)?;
        Ok(json!({
            "symbol": sym,
            "callers": neighbors.callers,
            "callees": neighbors.callees,
            "children": neighbors.children,
            "impact": impact,
        }))
    });
    match result {
        Ok(v) => Json(v).into_response(),
        Err(e) => fail(e),
    }
}

async fn api_leases(State(state): State<AppState>) -> Response {
    let store = state.store.lock().expect("store poisoned");
    match store.active_leases(None) {
        Ok(v) => Json(v).into_response(),
        Err(e) => fail(e),
    }
}

#[derive(Deserialize)]
struct AcquireBody {
    symbol: String,
    agent: String,
    task: Option<String>,
    note: Option<String>,
    #[serde(default = "default_ttl")]
    ttl_secs: i64,
    #[serde(default)]
    priority: i64,
    #[serde(default)]
    preempt: bool,
}

fn default_ttl() -> i64 {
    atlas_core::lease::DEFAULT_TTL_SECS
}

async fn api_acquire(State(state): State<AppState>, Json(body): Json<AcquireBody>) -> Response {
    let mut store = state.store.lock().expect("store poisoned");
    let opts = AcquireOptions {
        task: body.task,
        ttl_secs: body.ttl_secs,
        priority: body.priority,
        note: body.note,
        preempt: body.preempt,
        queue: true,
    };
    match store.acquire_ref(&body.symbol, &body.agent, &opts) {
        Ok((symbol, outcome)) => {
            let code = if outcome.is_granted() {
                StatusCode::OK
            } else {
                StatusCode::CONFLICT
            };
            (code, Json(json!({ "symbol": symbol, "result": outcome }))).into_response()
        }
        Err(e) => fail(e),
    }
}

#[derive(Deserialize)]
struct AgentQuery {
    agent: String,
}

async fn api_release(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<AgentQuery>,
) -> Response {
    let mut store = state.store.lock().expect("store poisoned");
    match store.release(&id, &q.agent) {
        Ok(l) => Json(l).into_response(),
        Err(e) => fail(e),
    }
}

async fn api_renew(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<AgentQuery>,
) -> Response {
    let mut store = state.store.lock().expect("store poisoned");
    match store.heartbeat(&q.agent, Some(&id)) {
        Ok(v) => Json(v).into_response(),
        Err(e) => fail(e),
    }
}

#[derive(Deserialize)]
struct EventQuery {
    #[serde(default)]
    since: i64,
    #[serde(default = "default_limit")]
    limit: usize,
}

async fn api_events(State(state): State<AppState>, Query(q): Query<EventQuery>) -> Response {
    let store = state.store.lock().expect("store poisoned");
    match store.events_since(q.since, q.limit) {
        Ok(v) => Json(v).into_response(),
        Err(e) => fail(e),
    }
}

async fn api_tasks(State(state): State<AppState>) -> Response {
    let store = state.store.lock().expect("store poisoned");
    match store.tasks() {
        Ok(v) => Json(v).into_response(),
        Err(e) => fail(e),
    }
}

#[derive(Deserialize)]
struct TransferBody {
    from: String,
    to: String,
}

async fn api_transfer(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<TransferBody>,
) -> Response {
    let mut store = state.store.lock().expect("store poisoned");
    match store.transfer_lease(&id, &body.from, &body.to) {
        Ok(l) => Json(l).into_response(),
        Err(e) => fail(e),
    }
}

#[derive(Deserialize)]
struct RequestQuery {
    agent: Option<String>,
    /// `inbox`, `outbox` or `all` (default).
    #[serde(default)]
    direction: Option<String>,
    #[serde(default)]
    all: bool,
}

async fn api_requests(State(state): State<AppState>, Query(q): Query<RequestQuery>) -> Response {
    let store = state.store.lock().expect("store poisoned");
    let direction = match q.direction.as_deref() {
        Some("inbox") => Direction::Inbox,
        Some("outbox") => Direction::Outbox,
        _ => Direction::All,
    };
    match store.requests(q.agent.as_deref(), direction, !q.all) {
        Ok(v) => Json(v).into_response(),
        Err(e) => fail(e),
    }
}

async fn api_request(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let store = state.store.lock().expect("store poisoned");
    match store.request(&id) {
        Ok(Some(r)) => Json(r).into_response(),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("no such request: {id}") })),
        )
            .into_response(),
        Err(e) => fail(e),
    }
}

#[derive(Deserialize)]
struct OpenRequestBody {
    #[serde(default = "default_kind")]
    kind: String,
    from: String,
    to: Option<String>,
    subject: String,
    #[serde(default)]
    body: Value,
    /// Symbol reference; resolved the same way the CLI resolves it.
    symbol: Option<String>,
    about_task: Option<String>,
    task: Option<String>,
    #[serde(default)]
    priority: i64,
    deadline_secs: Option<i64>,
}

fn default_kind() -> String {
    atlas_core::model::request_kind::QUESTION.to_string()
}

async fn api_open_request(
    State(state): State<AppState>,
    Json(body): Json<OpenRequestBody>,
) -> Response {
    let mut store = state.store.lock().expect("store poisoned");
    let symbol_id = match &body.symbol {
        Some(s) => match store.resolve(s) {
            Ok(sym) => Some(sym.id),
            Err(e) => return fail(e),
        },
        None => None,
    };
    // Asking for a held symbol routes to its holder and can perform a handover.
    if body.kind == atlas_core::model::request_kind::LEASE_TRANSFER {
        let Some(symbol_id) = symbol_id else {
            return fail(anyhow::anyhow!("lease-transfer needs a symbol"));
        };
        return match store.request_lease_transfer(
            &symbol_id,
            &body.from,
            body.body.get("reason").and_then(|v| v.as_str()),
            body.deadline_secs,
            body.priority,
            body.task.as_deref(),
        ) {
            Ok(r) => Json(r).into_response(),
            Err(e) => fail(e),
        };
    }
    let req = NewRequest {
        to: body.to,
        body: body.body,
        resource_symbol: symbol_id,
        resource_task: body.about_task,
        task: body.task,
        priority: body.priority,
        deadline_secs: body.deadline_secs,
        ..NewRequest::new(&body.kind, &body.from, &body.subject)
    };
    match store.open_request(&req) {
        Ok(r) => Json(r).into_response(),
        Err(e) => fail(e),
    }
}

#[derive(Deserialize)]
struct AnswerBody {
    agent: String,
    #[serde(default)]
    response: Option<Value>,
    reason: Option<String>,
}

async fn api_accept(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<AnswerBody>,
) -> Response {
    let mut store = state.store.lock().expect("store poisoned");
    match store.accept_request(&id, &body.agent, body.response) {
        Ok(r) => Json(r).into_response(),
        Err(e) => fail(e),
    }
}

async fn api_decline(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<AnswerBody>,
) -> Response {
    let mut store = state.store.lock().expect("store poisoned");
    match store.decline_request(&id, &body.agent, body.reason.as_deref()) {
        Ok(r) => Json(r).into_response(),
        Err(e) => fail(e),
    }
}

async fn api_fulfill(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<AnswerBody>,
) -> Response {
    let mut store = state.store.lock().expect("store poisoned");
    match store.fulfill_request(&id, &body.agent, body.response) {
        Ok(r) => Json(r).into_response(),
        Err(e) => fail(e),
    }
}

async fn api_progress(State(state): State<AppState>) -> Response {
    let store = state.store.lock().expect("store poisoned");
    match store.latest_progress() {
        Ok(v) => Json(v).into_response(),
        Err(e) => fail(e),
    }
}

#[derive(Deserialize)]
struct ProgressBody {
    agent: String,
    task: Option<String>,
    symbol: Option<String>,
    percent: Option<i64>,
    eta_secs: Option<i64>,
    note: Option<String>,
}

async fn api_post_progress(
    State(state): State<AppState>,
    Json(body): Json<ProgressBody>,
) -> Response {
    let mut store = state.store.lock().expect("store poisoned");
    let symbol_id = match &body.symbol {
        Some(s) => match store.resolve(s) {
            Ok(sym) => Some(sym.id),
            Err(e) => return fail(e),
        },
        None => None,
    };
    match store.record_progress(
        &body.agent,
        body.task.as_deref(),
        symbol_id.as_deref(),
        body.percent,
        body.eta_secs,
        body.note.as_deref(),
    ) {
        Ok(p) => Json(p).into_response(),
        Err(e) => fail(e),
    }
}

async fn api_endpoints(State(state): State<AppState>) -> Response {
    let store = state.store.lock().expect("store poisoned");
    match store.symbols_with_role(atlas_core::model::Role::Api) {
        Ok(v) => Json(v).into_response(),
        Err(e) => fail(e),
    }
}

async fn api_tables(State(state): State<AppState>) -> Response {
    let store = state.store.lock().expect("store poisoned");
    let result = store
        .list_symbols(None, Some(atlas_core::model::SymbolKind::Table), None, 500)
        .and_then(|tables| {
            let mut out = Vec::new();
            for t in tables {
                let accessed_by = store.neighbors(&t.id)?.callers;
                out.push(json!({ "table": t, "accessed_by": accessed_by }));
            }
            Ok(Value::Array(out))
        });
    match result {
        Ok(v) => Json(v).into_response(),
        Err(e) => fail(e),
    }
}

async fn api_services(State(state): State<AppState>) -> Response {
    let store = state.store.lock().expect("store poisoned");
    let result = store
        .list_symbols(None, Some(atlas_core::model::SymbolKind::Service), None, 200)
        .and_then(|services| {
            let mut out = Vec::new();
            for s in services {
                let n = store.neighbors(&s.id)?;
                let depends_on: Vec<String> = n
                    .callees
                    .iter()
                    .filter(|x| x.kind == atlas_core::model::EdgeKind::Imports)
                    .map(|x| x.symbol.name.clone())
                    .collect();
                out.push(json!({ "service": s, "depends_on": depends_on, "files": n.children.len() }));
            }
            Ok(Value::Array(out))
        });
    match result {
        Ok(v) => Json(v).into_response(),
        Err(e) => fail(e),
    }
}

#[derive(Deserialize)]
struct OwnersQuery {
    path: Option<String>,
}

async fn api_owners(State(state): State<AppState>, Query(q): Query<OwnersQuery>) -> Response {
    let owners = atlas_core::topology::Owners::load(&state.root);
    match q.path {
        Some(path) => Json(json!({
            "path": path,
            "codeowners": owners.owners_of(&path),
            "rule": owners.matching_rule(&path).map(|r| r.pattern.clone()),
        }))
        .into_response(),
        None => Json(json!({ "source": owners.source, "owners": owners.by_owner() })).into_response(),
    }
}

#[derive(Deserialize)]
struct ScheduleQuery {
    /// Work out dependencies from the code graph before planning.
    #[serde(default)]
    infer: bool,
}

async fn api_schedule(State(state): State<AppState>, Query(q): Query<ScheduleQuery>) -> Response {
    let mut store = state.store.lock().expect("store poisoned");
    store.sweep().ok();
    let inferred = if q.infer {
        match store.infer_dependencies() {
            Ok(v) => v,
            Err(e) => return fail(e),
        }
    } else {
        Vec::new()
    };
    match store.plan() {
        Ok(plan) => Json(json!({ "plan": plan, "inferred": inferred })).into_response(),
        Err(e) => fail(e),
    }
}

#[derive(Deserialize)]
struct ClaimBody {
    agent: String,
    #[serde(default = "default_ttl")]
    ttl_secs: i64,
}

async fn api_claim(State(state): State<AppState>, Json(body): Json<ClaimBody>) -> Response {
    let mut store = state.store.lock().expect("store poisoned");
    match store.claim_next(&body.agent, body.ttl_secs) {
        // 204: a well-formed request with nothing to hand out is not an error.
        Ok(None) => StatusCode::NO_CONTENT.into_response(),
        Ok(Some(t)) => Json(t).into_response(),
        Err(e) => fail(e),
    }
}

async fn api_agents(State(state): State<AppState>) -> Response {
    let store = state.store.lock().expect("store poisoned");
    match store.agents() {
        Ok(v) => Json(v).into_response(),
        Err(e) => fail(e),
    }
}

#[derive(Deserialize)]
struct PauseBody {
    #[serde(default)]
    paused: bool,
}

async fn api_pause(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(body): Json<PauseBody>,
) -> Response {
    let mut store = state.store.lock().expect("store poisoned");
    match store.set_agent_paused(&name, body.paused) {
        Ok(a) => Json(a).into_response(),
        Err(e) => fail(e),
    }
}

#[derive(Deserialize)]
struct PriorityBody {
    priority: i64,
}

async fn api_priority(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<PriorityBody>,
) -> Response {
    let mut store = state.store.lock().expect("store poisoned");
    match store.set_task_priority(&id, body.priority) {
        Ok(t) => Json(t).into_response(),
        Err(e) => fail(e),
    }
}

#[derive(Deserialize)]
struct AssignBody {
    to: String,
    /// Who is doing the reassigning; recorded on the event.
    actor: Option<String>,
    /// Take unclaimed-but-contended scope from a lower-priority holder.
    #[serde(default)]
    preempt: bool,
    #[serde(default)]
    priority: i64,
}

async fn api_assign(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<AssignBody>,
) -> Response {
    let mut store = state.store.lock().expect("store poisoned");
    match store.assign_task_opts(&id, &body.to, body.actor.as_deref(), body.preempt, body.priority) {
        Ok(t) => Json(t).into_response(),
        Err(e) => fail(e),
    }
}

#[derive(Deserialize)]
struct TaskStateBody {
    state: String,
    agent: Option<String>,
    note: Option<String>,
    #[serde(default)]
    force: bool,
}

async fn api_task_state(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<TaskStateBody>,
) -> Response {
    let Some(target) = atlas_core::model::TaskState::parse(&body.state) else {
        return fail(anyhow::anyhow!("unknown task state: {}", body.state));
    };
    let mut store = state.store.lock().expect("store poisoned");
    match store.set_task_state(
        &id,
        target,
        body.agent.as_deref(),
        body.note.as_deref(),
        body.force,
    ) {
        Ok(t) => Json(t).into_response(),
        Err(e) => fail(e),
    }
}

#[derive(Deserialize)]
struct ThroughputQuery {
    #[serde(default = "default_window")]
    minutes: i64,
    #[serde(default = "default_buckets")]
    buckets: usize,
}

fn default_window() -> i64 {
    30
}

fn default_buckets() -> usize {
    30
}

async fn api_throughput(
    State(state): State<AppState>,
    Query(q): Query<ThroughputQuery>,
) -> Response {
    let store = state.store.lock().expect("store poisoned");
    match store.throughput(q.minutes, q.buckets) {
        Ok(t) => Json(t).into_response(),
        Err(e) => fail(e),
    }
}

async fn api_repos(State(state): State<AppState>) -> Response {
    let store = state.store.lock().expect("store poisoned");
    match store.repos() {
        Ok(v) => Json(v).into_response(),
        Err(e) => fail(e),
    }
}

async fn api_goals(State(state): State<AppState>) -> Response {
    let store = state.store.lock().expect("store poisoned");
    match store.goals() {
        Ok(v) => Json(v).into_response(),
        Err(e) => fail(e),
    }
}

#[derive(Deserialize)]
struct AddGoalBody {
    title: String,
    #[serde(default)]
    priority: i64,
    description: Option<String>,
    created_by: Option<String>,
}

async fn api_add_goal(State(state): State<AppState>, Json(body): Json<AddGoalBody>) -> Response {
    let mut store = state.store.lock().expect("store poisoned");
    match store.add_goal(
        &body.title,
        body.priority,
        body.description.as_deref(),
        body.created_by.as_deref(),
    ) {
        Ok(g) => Json(g).into_response(),
        Err(e) => fail(e),
    }
}

async fn api_goal(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let store = state.store.lock().expect("store poisoned");
    match store.goal(&id) {
        Ok(Some(g)) => Json(g).into_response(),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("no such goal: {id}") })),
        )
            .into_response(),
        Err(e) => fail(e),
    }
}

async fn api_goal_progress(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let store = state.store.lock().expect("store poisoned");
    let result = store.goal_progress(&id).and_then(|progress| {
        let tasks = store.goal_tasks(&id)?;
        Ok(json!({ "progress": progress, "tasks": tasks }))
    });
    match result {
        Ok(v) => Json(v).into_response(),
        Err(e) => fail(e),
    }
}

#[derive(Deserialize)]
struct DecomposeBody {
    task: String,
    #[serde(default)]
    priority: i64,
    #[serde(default)]
    deps: Vec<String>,
    #[serde(default)]
    symbols: Vec<String>,
}

async fn api_goal_decompose(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<DecomposeBody>,
) -> Response {
    let mut store = state.store.lock().expect("store poisoned");
    match store.goal_decompose(&id, &body.task, body.priority, &body.deps, &body.symbols) {
        Ok(t) => Json(t).into_response(),
        Err(e) => fail(e),
    }
}

#[derive(Deserialize)]
struct SuggestQuery {
    near: String,
    #[serde(default = "default_depth")]
    depth: usize,
}

async fn api_goal_suggest(
    State(state): State<AppState>,
    Path(_id): Path<String>,
    Query(q): Query<SuggestQuery>,
) -> Response {
    let store = state.store.lock().expect("store poisoned");
    match store.suggest_decomposition(&q.near, q.depth) {
        Ok(v) => Json(v).into_response(),
        Err(e) => fail(e),
    }
}

#[derive(Deserialize)]
struct PlanQuery {
    #[serde(default = "default_depth")]
    depth: usize,
}

async fn api_goal_plan(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<PlanQuery>,
) -> Response {
    let store = state.store.lock().expect("store poisoned");
    match store.plan_goal(&id, q.depth) {
        Ok(v) => Json(v).into_response(),
        Err(e) => fail(e),
    }
}

async fn api_review_submit(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<AgentQuery>,
) -> Response {
    let mut store = state.store.lock().expect("store poisoned");
    match store.submit_for_review(&id, &body.agent) {
        Ok(t) => Json(t).into_response(),
        Err(e) => fail(e),
    }
}

#[derive(Deserialize)]
struct ApproveBody {
    agent: String,
    #[serde(default)]
    force: bool,
}

async fn api_review_approve(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<ApproveBody>,
) -> Response {
    let mut store = state.store.lock().expect("store poisoned");
    match store.approve_review(&id, &body.agent, body.force) {
        Ok(t) => Json(t).into_response(),
        Err(e) => fail(e),
    }
}

#[derive(Deserialize)]
struct RejectBody {
    agent: String,
    reason: Option<String>,
}

async fn api_review_reject(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<RejectBody>,
) -> Response {
    let mut store = state.store.lock().expect("store poisoned");
    match store.reject_review(&id, &body.agent, body.reason.as_deref()) {
        Ok(t) => Json(t).into_response(),
        Err(e) => fail(e),
    }
}

#[derive(Deserialize)]
struct ObserveQuery {
    goal: Option<String>,
}

async fn api_observe(State(state): State<AppState>, Query(q): Query<ObserveQuery>) -> Response {
    let mut store = state.store.lock().expect("store poisoned");
    store.sweep().ok();
    let result = (|| {
        let agents = store.agents()?;
        let tasks = match &q.goal {
            Some(g) => store.goal_tasks(g)?,
            None => store.tasks()?,
        };
        let plan = store.plan()?;
        anyhow::Ok(json!({ "agents": agents, "tasks": tasks, "plan": plan }))
    })();
    match result {
        Ok(v) => Json(v).into_response(),
        Err(e) => fail(e),
    }
}

#[derive(Deserialize)]
struct ContinueBody {
    agent: String,
    #[serde(default = "default_ttl")]
    ttl_secs: i64,
    goal: Option<String>,
}

async fn api_continue(State(state): State<AppState>, Json(body): Json<ContinueBody>) -> Response {
    let mut store = state.store.lock().expect("store poisoned");
    let already_working = match store.agents() {
        Ok(agents) => agents
            .into_iter()
            .find(|a| a.agent.name == body.agent)
            .and_then(|a| a.current_task),
        Err(e) => return fail(e),
    };
    if already_working.is_some() {
        return match store.heartbeat(&body.agent, None) {
            Ok(renewed) => Json(json!({ "task": already_working, "renewed": renewed })).into_response(),
            Err(e) => fail(e),
        };
    }
    match store.claim_next_in(&body.agent, body.ttl_secs, body.goal.as_deref()) {
        Ok(None) => StatusCode::NO_CONTENT.into_response(),
        Ok(Some(t)) => Json(t).into_response(),
        Err(e) => fail(e),
    }
}

async fn api_check(State(state): State<AppState>, Query(q): Query<AgentQuery>) -> Response {
    let store = state.store.lock().expect("store poisoned");
    match workspace::check_workspace(&store, &state.root, &q.agent, &[]) {
        Ok(r) => Json(r).into_response(),
        Err(e) => fail(e),
    }
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ScanBody {
    /// Empty means the whole workspace. Note that a full scan deliberately
    /// does not notify (see `notify.rs`) — pass the paths that changed to get
    /// api-change notices out to whoever is affected.
    paths: Vec<PathBuf>,
    force: bool,
}

async fn api_scan(State(state): State<AppState>, body: Option<Json<ScanBody>>) -> Response {
    let body = body.map(|Json(b)| b).unwrap_or_default();
    let mut store = state.store.lock().expect("store poisoned");
    // `scan_workspace`, not bare `scan`: this route used to skip notification
    // entirely, so a scan triggered over HTTP silently failed to tell anyone
    // their API had moved, while the identical scan from the CLI or the file
    // watcher did. It also covers every registered repo rather than just R1.
    match workspace::scan_workspace(&mut store, &state.root, &body.paths, body.force) {
        Ok(stats) => Json(stats).into_response(),
        Err(e) => fail(e),
    }
}

// ------------------------------------------------------------------- guard

#[derive(Deserialize)]
struct GuardQuery {
    path: String,
    agent: String,
    symbol: Option<String>,
}

async fn api_guard(State(state): State<AppState>, Query(q): Query<GuardQuery>) -> Response {
    let store = state.store.lock().expect("store poisoned");
    match workspace::guard_workspace(
        &store,
        &state.root,
        &q.agent,
        std::path::Path::new(&q.path),
        q.symbol.as_deref(),
    ) {
        Ok(r) => Json(r).into_response(),
        Err(e) => fail(e),
    }
}

// ----------------------------------------------------------------- context

#[derive(Deserialize)]
struct ContextQuery {
    task: Option<String>,
    agent: Option<String>,
    #[serde(default = "default_depth")]
    depth: usize,
}

async fn api_context(State(state): State<AppState>, Query(q): Query<ContextQuery>) -> Response {
    let store = state.store.lock().expect("store poisoned");
    let out = match (&q.task, &q.agent) {
        (Some(t), _) => store.task_context(t, q.depth).map(|c| json!(c)),
        (None, Some(a)) => store.agent_context(a, q.depth).map(|c| json!(c)),
        (None, None) => {
            return fail(anyhow::anyhow!("pass task= or agent="));
        }
    };
    match out {
        Ok(v) => Json(v).into_response(),
        Err(e) => fail(e),
    }
}

// ------------------------------------------------- architecture & activity

#[derive(Deserialize)]
struct ArchQuery {
    repo: Option<String>,
    #[serde(default = "default_arch_depth")]
    depth: usize,
}

fn default_arch_depth() -> usize {
    1
}

async fn api_arch(State(state): State<AppState>, Query(q): Query<ArchQuery>) -> Response {
    let mut store = state.store.lock().expect("store poisoned");
    // The overlays are the point of this graph, and a stale "alice is editing
    // here" makes the picture worse than no picture.
    store.sweep().ok();
    match store.architecture(q.repo.as_deref(), q.depth) {
        Ok(g) => Json(g).into_response(),
        Err(e) => fail(e),
    }
}

async fn api_arch_node(
    State(state): State<AppState>,
    Path(node): Path<String>,
    Query(q): Query<ArchQuery>,
) -> Response {
    let mut store = state.store.lock().expect("store poisoned");
    store.sweep().ok();
    match store.arch_node(&node, q.depth) {
        Ok(d) => Json(d).into_response(),
        Err(e) => (StatusCode::NOT_FOUND, Json(json!({ "error": e.to_string() }))).into_response(),
    }
}

#[derive(Deserialize)]
struct ActivityQuery {
    agent: Option<String>,
    #[serde(default)]
    all: bool,
}

async fn api_activity(State(state): State<AppState>, Query(q): Query<ActivityQuery>) -> Response {
    let mut store = state.store.lock().expect("store poisoned");
    store.sweep().ok();
    let result = match (&q.agent, q.all) {
        (Some(a), _) => store.activity_for_agent(a),
        (None, true) => store.all_activity(100),
        (None, false) => store.live_activity(),
    };
    match result {
        Ok(v) => Json(v).into_response(),
        Err(e) => fail(e),
    }
}

#[derive(Deserialize)]
struct NoticeQuery {
    agent: Option<String>,
    #[serde(default = "default_notice_limit")]
    limit: usize,
}

fn default_notice_limit() -> usize {
    25
}

async fn api_notifications(
    State(state): State<AppState>,
    Query(q): Query<NoticeQuery>,
) -> Response {
    let mut store = state.store.lock().expect("store poisoned");
    store.sweep().ok();
    match store.notifications(q.agent.as_deref(), q.limit) {
        Ok(v) => Json(v).into_response(),
        Err(e) => fail(e),
    }
}

// ---------------------------------------------------------------- sessions

#[derive(Deserialize)]
struct SessionQuery {
    #[serde(default)]
    live: bool,
}

async fn api_sessions(State(state): State<AppState>, Query(q): Query<SessionQuery>) -> Response {
    let mut store = state.store.lock().expect("store poisoned");
    store.sweep().ok();
    match store.sessions(q.live) {
        Ok(s) => Json(s).into_response(),
        Err(e) => fail(e),
    }
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct EndSessionBody {
    keep_leases: bool,
}

async fn api_end_session(
    State(state): State<AppState>,
    Path(id): Path<String>,
    body: Option<Json<EndSessionBody>>,
) -> Response {
    let body = body.map(|Json(b)| b).unwrap_or_default();
    let mut store = state.store.lock().expect("store poisoned");
    match store.end_session(&id, !body.keep_leases) {
        Ok(s) => Json(s).into_response(),
        Err(e) => fail(e),
    }
}

// ------------------------------------------------------------------ memory

#[derive(Deserialize)]
struct MemoryQuery {
    tag: Option<String>,
}

async fn api_memory(State(state): State<AppState>, Query(q): Query<MemoryQuery>) -> Response {
    let store = state.store.lock().expect("store poisoned");
    match store.memory_list(q.tag.as_deref()) {
        Ok(m) => Json(m).into_response(),
        Err(e) => fail(e),
    }
}

async fn api_memory_get(State(state): State<AppState>, Path(key): Path<String>) -> Response {
    let store = state.store.lock().expect("store poisoned");
    match store.memory_get(&key) {
        Ok(Some(m)) => Json(m).into_response(),
        Ok(None) => (StatusCode::NOT_FOUND, Json(json!({ "error": "no such key" }))).into_response(),
        Err(e) => fail(e),
    }
}

#[derive(Deserialize)]
struct MemoryBody {
    key: String,
    value: String,
    author: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
}

async fn api_memory_set(State(state): State<AppState>, Json(body): Json<MemoryBody>) -> Response {
    let mut store = state.store.lock().expect("store poisoned");
    // `memory_set` returns nothing, so read it back: a caller that just wrote
    // an entry should get the stored entry, not `null`.
    let written = store
        .memory_set(&body.key, &body.value, body.author.as_deref(), &body.tags)
        .and_then(|_| store.memory_get(&body.key));
    match written {
        Ok(m) => Json(m).into_response(),
        Err(e) => fail(e),
    }
}

async fn api_memory_delete(State(state): State<AppState>, Path(key): Path<String>) -> Response {
    let mut store = state.store.lock().expect("store poisoned");
    match store.memory_delete(&key) {
        Ok(removed) => Json(json!({ "key": key, "removed": removed })).into_response(),
        Err(e) => fail(e),
    }
}

// ------------------------------------------------------------- agent admin

#[derive(Deserialize)]
struct CapabilityBody {
    /// `null` makes the agent a generalist again.
    capability: Option<String>,
}

async fn api_capability(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(body): Json<CapabilityBody>,
) -> Response {
    let capability = match body.capability.as_deref() {
        None | Some("") => None,
        Some(c) => match atlas_core::model::Capability::parse(c) {
            Some(c) => Some(c),
            None => return fail(anyhow::anyhow!("unknown capability: {c}")),
        },
    };
    let mut store = state.store.lock().expect("store poisoned");
    match store.set_agent_capability(&name, capability) {
        Ok(a) => Json(a).into_response(),
        Err(e) => fail(e),
    }
}

async fn api_agent_leave(State(state): State<AppState>, Path(name): Path<String>) -> Response {
    let mut store = state.store.lock().expect("store poisoned");
    match store.deregister_agent(&name) {
        Ok(released) => Json(json!({ "agent": name, "leases_released": released })).into_response(),
        Err(e) => fail(e),
    }
}

#[derive(Deserialize)]
struct ScopeBody {
    symbols: Vec<String>,
}

async fn api_task_scope(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<ScopeBody>,
) -> Response {
    let mut store = state.store.lock().expect("store poisoned");
    match store.set_task_scope(&id, &body.symbols) {
        Ok(s) => Json(s).into_response(),
        Err(e) => fail(e),
    }
}

/// Convenience for embedding: the JSON a dashboard needs in one call.
pub fn snapshot(store: &Store) -> Result<Value> {
    Ok(serde_json::to_value(store.status(30)?)?)
}
