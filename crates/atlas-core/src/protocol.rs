//! Agent-to-agent coordination.
//!
//! Phase 0-1 let agents avoid each other. This is how they *negotiate*: a
//! typed request/response protocol with deadlines, so an agent that is blocked
//! can ask the agent blocking it and get a machine-readable answer — no human
//! reading a chat log and brokering the handoff.
//!
//! Three things make it work without a human:
//!
//! - **Accepting a `lease-transfer` performs the transfer**, atomically, so
//!   ownership moves hand to hand and no third agent can snipe the symbol in
//!   the gap between release and re-acquire.
//! - **Requests self-resolve.** Release a symbol somebody asked for and their
//!   request is fulfilled; finish a task somebody is blocked on and their
//!   dependency clears. The holder does not have to remember who was waiting.
//! - **Deadlines expire.** A request nobody answers dies on its own, exactly
//!   like a lease, so a waiting agent is never waiting forever.

use anyhow::{anyhow, bail, Result};
use rusqlite::{params, OptionalExtension, Row, Transaction};
use serde_json::{json, Value};

use crate::ids;
use crate::lease;
use crate::model::*;
use crate::store::{self, Store};

const REQUEST_SELECT: &str = "SELECT r.id, r.kind, r.from_agent, r.to_agent, r.subject, r.body, \
    r.resource_symbol, \
    CASE WHEN s.kind = 'file' THEN s.path ELSE s.path || ':' || s.fqn END, \
    r.resource_task, r.task, r.priority, r.state, r.created_at, r.deadline_at, \
    r.resolved_at, r.resolver, r.response \
    FROM requests r LEFT JOIN symbols s ON s.id = r.resource_symbol";

/// Everything needed to open a request. Only `kind`, `from` and `subject` are
/// really required; the rest sharpen it.
#[derive(Debug, Clone)]
pub struct NewRequest {
    pub kind: String,
    pub from: String,
    /// `None` broadcasts to the whole workspace.
    pub to: Option<String>,
    pub subject: String,
    pub body: Value,
    pub resource_symbol: Option<String>,
    pub resource_task: Option<String>,
    pub task: Option<String>,
    pub priority: i64,
    /// Seconds from now. `None` means "no deadline", which is rarely wise.
    pub deadline_secs: Option<i64>,
}

impl NewRequest {
    pub fn new(kind: &str, from: &str, subject: &str) -> NewRequest {
        NewRequest {
            kind: kind.to_string(),
            from: from.to_string(),
            to: None,
            subject: subject.to_string(),
            body: json!({}),
            resource_symbol: None,
            resource_task: None,
            task: None,
            priority: 0,
            deadline_secs: None,
        }
    }
}

/// Which side of the conversation to list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// Addressed to me, plus broadcasts.
    Inbox,
    /// Opened by me.
    Outbox,
    /// Everything in the workspace.
    All,
}

impl Store {
    /// Open a request. Broadcast (`to: None`) is legitimate: "does anyone own
    /// the payments schema?" has no obvious recipient.
    pub fn open_request(&mut self, req: &NewRequest) -> Result<Request> {
        let req = req.clone();
        self.write(move |tx| open_request_tx(tx, &req))
    }

    /// Ask whoever currently holds `symbol_id` to hand it over.
    ///
    /// The point of routing this through the runtime rather than the requester
    /// guessing: the runtime knows who holds it, what for, and for how long.
    pub fn request_lease_transfer(
        &mut self,
        symbol_id: &str,
        from: &str,
        reason: Option<&str>,
        deadline_secs: Option<i64>,
        priority: i64,
        task: Option<&str>,
    ) -> Result<Request> {
        let holder = self
            .active_leases(None)?
            .into_iter()
            .find(|l| l.symbol_id == symbol_id)
            .ok_or_else(|| anyhow!("nobody holds that symbol — just acquire it"))?;
        if holder.agent == from {
            bail!("{from} already holds that lease");
        }
        let symbol = self
            .symbol(symbol_id)?
            .ok_or_else(|| anyhow!("unknown symbol: {symbol_id}"))?;

        self.open_request(&NewRequest {
            kind: request_kind::LEASE_TRANSFER.to_string(),
            from: from.to_string(),
            to: Some(holder.agent.clone()),
            subject: format!("hand over {}", symbol.handle()),
            body: json!({
                "reason": reason,
                "current_task": holder.task,
                "holder_expires_in_secs": holder.seconds_left(ids::now_ms()).max(0),
            }),
            resource_symbol: Some(symbol_id.to_string()),
            resource_task: None,
            task: task.map(|s| s.to_string()),
            priority,
            deadline_secs,
        })
    }

    pub fn request(&self, id: &str) -> Result<Option<Request>> {
        Ok(self
            .conn()
            .query_row(
                &format!("{REQUEST_SELECT} WHERE r.id = ?1"),
                params![id],
                row_to_request,
            )
            .optional()?)
    }

    /// List requests. `live_only` hides everything already answered.
    pub fn requests(
        &self,
        agent: Option<&str>,
        direction: Direction,
        live_only: bool,
    ) -> Result<Vec<Request>> {
        let mut sql = format!("{REQUEST_SELECT} WHERE 1=1");
        let mut args: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        match (direction, agent) {
            (Direction::Inbox, Some(a)) => {
                sql.push_str(" AND (r.to_agent = ? OR r.to_agent IS NULL) AND r.from_agent != ?");
                args.push(Box::new(a.to_string()));
                args.push(Box::new(a.to_string()));
            }
            (Direction::Outbox, Some(a)) => {
                sql.push_str(" AND r.from_agent = ?");
                args.push(Box::new(a.to_string()));
            }
            _ => {}
        }
        if live_only {
            sql.push_str(" AND r.state IN ('open', 'accepted')");
        }
        sql.push_str(" ORDER BY r.priority DESC, r.seq ASC");

        let mut stmt = self.conn().prepare(&sql)?;
        let refs: Vec<&dyn rusqlite::ToSql> = args.iter().map(|b| b.as_ref()).collect();
        let rows = stmt.query_map(refs.as_slice(), row_to_request)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Say yes.
    ///
    /// For `lease-transfer` this *is* the handover: the lease moves to the
    /// requester inside the same transaction and the request lands
    /// `fulfilled`. For every other kind it means "I have taken this on",
    /// and the requester waits for [`Store::fulfill_request`].
    pub fn accept_request(
        &mut self,
        id: &str,
        agent: &str,
        response: Option<Value>,
    ) -> Result<Request> {
        let id = id.to_string();
        let agent = agent.to_string();
        self.write(move |tx| {
            expire_requests_tx(tx)?;
            let req = load_request(tx, &id)?.ok_or_else(|| anyhow!("no such request: {id}"))?;
            guard_responder(&req, &agent, true)?;

            if req.kind == request_kind::LEASE_TRANSFER {
                let symbol_id = req
                    .resource_symbol
                    .clone()
                    .ok_or_else(|| anyhow!("lease-transfer request names no symbol"))?;
                let held = lease::active_lease_for(tx, &symbol_id, &agent)?.ok_or_else(|| {
                    anyhow!("{agent} no longer holds {}", req.resource_label())
                })?;
                let moved = lease::transfer_lease_tx(tx, &held.id, &agent, &req.from)?;
                let response = response.unwrap_or_else(|| json!({}));
                let response = merge(
                    response,
                    json!({ "lease": moved.id, "transferred_to": req.from }),
                );
                resolve_tx(tx, &req, RequestState::Fulfilled, &agent, Some(response))?;
            } else {
                resolve_tx(tx, &req, RequestState::Accepted, &agent, response)?;
            }
            load_request(tx, &id)?.ok_or_else(|| anyhow!("request vanished"))
        })
    }

    /// Say no — with a reason, because "no, I'm mid-refactor, 90 seconds" is
    /// actionable and a bare refusal is not.
    pub fn decline_request(&mut self, id: &str, agent: &str, reason: Option<&str>) -> Result<Request> {
        let id = id.to_string();
        let agent = agent.to_string();
        let reason = reason.map(|s| s.to_string());
        self.write(move |tx| {
            let req = load_request(tx, &id)?.ok_or_else(|| anyhow!("no such request: {id}"))?;
            guard_responder(&req, &agent, false)?;
            resolve_tx(
                tx,
                &req,
                RequestState::Declined,
                &agent,
                Some(json!({ "reason": reason })),
            )?;
            load_request(tx, &id)?.ok_or_else(|| anyhow!("request vanished"))
        })
    }

    /// Deliver: the interface exists, the dependency is met, the answer is
    /// attached. This is the state a waiting agent is polling for.
    pub fn fulfill_request(
        &mut self,
        id: &str,
        agent: &str,
        response: Option<Value>,
    ) -> Result<Request> {
        let id = id.to_string();
        let agent = agent.to_string();
        self.write(move |tx| {
            let req = load_request(tx, &id)?.ok_or_else(|| anyhow!("no such request: {id}"))?;
            guard_responder(&req, &agent, false)?;
            resolve_tx(tx, &req, RequestState::Fulfilled, &agent, response)?;
            load_request(tx, &id)?.ok_or_else(|| anyhow!("request vanished"))
        })
    }

    /// Withdraw your own request.
    pub fn cancel_request(&mut self, id: &str, agent: &str) -> Result<Request> {
        let id = id.to_string();
        let agent = agent.to_string();
        self.write(move |tx| {
            let req = load_request(tx, &id)?.ok_or_else(|| anyhow!("no such request: {id}"))?;
            if req.from != agent {
                bail!("only {} can cancel that request", req.from);
            }
            if !req.state.is_live() {
                bail!("request is already {}", req.state.as_str());
            }
            resolve_tx(tx, &req, RequestState::Cancelled, &agent, None)?;
            load_request(tx, &id)?.ok_or_else(|| anyhow!("request vanished"))
        })
    }

    /// Retire requests whose deadline passed. Called from the same sweep as
    /// lease expiry, so an unanswered ask cannot block an agent forever.
    pub fn expire_requests(&mut self) -> Result<usize> {
        self.write(expire_requests_tx)
    }

    // ------------------------------------------------------------- progress

    /// Publish where you are. Doubles as a heartbeat: an agent reporting
    /// progress is self-evidently alive, so its leases get renewed too.
    pub fn record_progress(
        &mut self,
        agent: &str,
        task: Option<&str>,
        symbol_id: Option<&str>,
        percent: Option<i64>,
        eta_secs: Option<i64>,
        note: Option<&str>,
    ) -> Result<ProgressUpdate> {
        let agent_owned = agent.to_string();
        let task = task.map(|s| s.to_string());
        let symbol_id = symbol_id.map(|s| s.to_string());
        let note = note.map(|s| s.to_string());
        let id = self.write(move |tx| {
            let now = ids::now_ms();
            let percent = percent.map(|p| p.clamp(0, 100));
            tx.execute(
                "INSERT INTO progress(agent, task, symbol_id, percent, eta_secs, note, ts) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![agent_owned, task, symbol_id, percent, eta_secs, note, now],
            )?;
            let id = tx.last_insert_rowid();
            let handle = match &symbol_id {
                Some(s) => store::tx_symbol(tx, s)?.map(|s| s.handle()),
                None => None,
            };
            store::emit(
                tx,
                "agent.progress",
                Some(&agent_owned),
                handle.as_deref(),
                task.as_deref(),
                json!({ "percent": percent, "eta_secs": eta_secs, "note": note }),
            )?;
            tx.execute(
                "UPDATE agents SET heartbeat_at = ?2 WHERE name = ?1",
                params![agent_owned, now],
            )?;
            Ok(id)
        })?;
        // Reporting progress means the agent is alive; keep its work reserved.
        self.heartbeat(agent, None)?;
        self.progress(id)?
            .ok_or_else(|| anyhow!("progress vanished"))
    }

    fn progress(&self, id: i64) -> Result<Option<ProgressUpdate>> {
        Ok(self
            .conn()
            .query_row(
                &format!("{PROGRESS_SELECT} WHERE p.id = ?1"),
                params![id],
                row_to_progress,
            )
            .optional()?)
    }

    /// The most recent update per agent — what a dashboard or a planner wants.
    pub fn latest_progress(&self) -> Result<Vec<ProgressUpdate>> {
        let mut stmt = self.conn().prepare(&format!(
            "{PROGRESS_SELECT} WHERE p.id IN (SELECT MAX(id) FROM progress GROUP BY agent) \
             ORDER BY p.ts DESC"
        ))?;
        let rows = stmt.query_map([], row_to_progress)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    pub fn progress_for(&self, agent: &str, limit: usize) -> Result<Vec<ProgressUpdate>> {
        let mut stmt = self.conn().prepare(&format!(
            "{PROGRESS_SELECT} WHERE p.agent = ?1 ORDER BY p.ts DESC LIMIT ?2"
        ))?;
        let rows = stmt.query_map(params![agent, limit as i64], row_to_progress)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }
}

const PROGRESS_SELECT: &str = "SELECT p.id, p.agent, p.task, p.symbol_id, \
    CASE WHEN s.kind = 'file' THEN s.path ELSE s.path || ':' || s.fqn END, \
    p.percent, p.eta_secs, p.note, p.ts \
    FROM progress p LEFT JOIN symbols s ON s.id = p.symbol_id";

// ------------------------------------------------------------------ internals

fn open_request_tx(tx: &Transaction, req: &NewRequest) -> Result<Request> {
    if req.to.as_deref() == Some(req.from.as_str()) {
        bail!("an agent cannot send a request to itself");
    }
    let now = ids::now_ms();
    let id = ids::unique_id("req");
    let deadline = req.deadline_secs.map(|s| now + s * 1000);

    tx.execute(
        "INSERT INTO requests(id, seq, kind, from_agent, to_agent, subject, body, \
             resource_symbol, resource_task, task, priority, state, created_at, deadline_at, \
             resolved_at, resolver, response) \
         VALUES (?1, (SELECT COALESCE(MAX(seq), 0) + 1 FROM requests), ?2, ?3, ?4, ?5, ?6, \
             ?7, ?8, ?9, ?10, 'open', ?11, ?12, NULL, NULL, NULL)",
        params![
            id,
            req.kind,
            req.from,
            req.to,
            req.subject,
            req.body.to_string(),
            req.resource_symbol,
            req.resource_task,
            req.task,
            req.priority,
            now,
            deadline
        ],
    )?;

    let handle = match &req.resource_symbol {
        Some(s) => store::tx_symbol(tx, s)?.map(|s| s.handle()),
        None => None,
    };
    store::emit(
        tx,
        "request.opened",
        Some(&req.from),
        handle.as_deref(),
        req.task.as_deref(),
        json!({
            "request": id,
            "kind": req.kind,
            "to": req.to,
            "subject": req.subject,
            "deadline_at": deadline,
        }),
    )?;
    load_request(tx, &id)?.ok_or_else(|| anyhow!("request vanished"))
}

/// Only the addressee may answer a directed request; anyone but the sender may
/// answer a broadcast.
///
/// `require_open` distinguishes accepting (a commitment you make once) from
/// delivering or backing out (both legal after you have accepted).
fn guard_responder(req: &Request, agent: &str, require_open: bool) -> Result<()> {
    if !req.state.is_live() {
        bail!("request {} is already {}", req.id, req.state.as_str());
    }
    if require_open && req.state != RequestState::Open {
        bail!("request {} is already {}", req.id, req.state.as_str());
    }
    match &req.to {
        Some(to) if to != agent => bail!("request {} is addressed to {to}, not {agent}", req.id),
        _ => {}
    }
    if req.from == agent {
        bail!("{agent} opened that request; use cancel instead");
    }
    Ok(())
}

fn resolve_tx(
    tx: &Transaction,
    req: &Request,
    state: RequestState,
    resolver: &str,
    response: Option<Value>,
) -> Result<()> {
    let terminal = state != RequestState::Accepted;
    tx.execute(
        "UPDATE requests SET state = ?2, resolver = ?3, response = COALESCE(?4, response), \
             resolved_at = CASE WHEN ?5 THEN ?6 ELSE resolved_at END WHERE id = ?1",
        params![
            req.id,
            state.as_str(),
            resolver,
            response.as_ref().map(|v| v.to_string()),
            terminal,
            ids::now_ms()
        ],
    )?;
    let kind = match state {
        RequestState::Accepted => "request.accepted",
        RequestState::Declined => "request.declined",
        RequestState::Fulfilled => "request.fulfilled",
        RequestState::Cancelled => "request.cancelled",
        RequestState::Expired => "request.expired",
        RequestState::Open => "request.reopened",
    };
    store::emit(
        tx,
        kind,
        Some(resolver),
        req.resource_handle.as_deref(),
        req.task.as_deref(),
        json!({
            "request": req.id,
            "kind": req.kind,
            "requester": req.from,
            "response": response,
        }),
    )?;
    Ok(())
}

fn expire_requests_tx(tx: &Transaction) -> Result<usize> {
    let now = ids::now_ms();
    let mut stmt = tx.prepare(&format!(
        "{REQUEST_SELECT} WHERE r.state IN ('open', 'accepted') \
         AND r.deadline_at IS NOT NULL AND r.deadline_at <= ?1"
    ))?;
    let due: Vec<Request> = stmt
        .query_map(params![now], row_to_request)?
        .collect::<Result<Vec<_>, _>>()?;
    drop(stmt);

    for req in &due {
        let from = req.from.clone();
        resolve_tx(
            tx,
            req,
            RequestState::Expired,
            &from,
            Some(json!({ "reason": "deadline passed with no answer" })),
        )?;
    }
    Ok(due.len())
}

/// Releasing a symbol answers everyone who asked for it. Called from the lease
/// layer so a holder never has to remember who was waiting.
pub(crate) fn resolve_symbol_requests(
    tx: &Transaction,
    symbol_id: &str,
    reason: &str,
) -> Result<usize> {
    let mut stmt = tx.prepare(&format!(
        "{REQUEST_SELECT} WHERE r.resource_symbol = ?1 AND r.state IN ('open', 'accepted') \
         AND r.kind = ?2"
    ))?;
    let open: Vec<Request> = stmt
        .query_map(params![symbol_id, request_kind::LEASE_TRANSFER], row_to_request)?
        .collect::<Result<Vec<_>, _>>()?;
    drop(stmt);

    for req in &open {
        let resolver = req.to.clone().unwrap_or_else(|| req.from.clone());
        resolve_tx(
            tx,
            req,
            RequestState::Fulfilled,
            &resolver,
            Some(json!({ "reason": reason, "symbol_free": true })),
        )?;
    }
    Ok(open.len())
}

/// Finishing a task clears every dependency request waiting on it.
pub(crate) fn resolve_task_requests(tx: &Transaction, task_id: &str, reason: &str) -> Result<usize> {
    let mut stmt = tx.prepare(&format!(
        "{REQUEST_SELECT} WHERE r.resource_task = ?1 AND r.state IN ('open', 'accepted')"
    ))?;
    let open: Vec<Request> = stmt
        .query_map(params![task_id], row_to_request)?
        .collect::<Result<Vec<_>, _>>()?;
    drop(stmt);

    for req in &open {
        let resolver = req.to.clone().unwrap_or_else(|| req.from.clone());
        resolve_tx(
            tx,
            req,
            RequestState::Fulfilled,
            &resolver,
            Some(json!({ "reason": reason, "task": task_id })),
        )?;
    }
    Ok(open.len())
}

pub(crate) fn load_request(tx: &Transaction, id: &str) -> Result<Option<Request>> {
    Ok(tx
        .query_row(
            &format!("{REQUEST_SELECT} WHERE r.id = ?1"),
            params![id],
            row_to_request,
        )
        .optional()?)
}

fn merge(base: Value, extra: Value) -> Value {
    match (base, extra) {
        (Value::Object(mut a), Value::Object(b)) => {
            a.extend(b);
            Value::Object(a)
        }
        (_, extra) => extra,
    }
}

impl Request {
    fn resource_label(&self) -> String {
        self.resource_handle
            .clone()
            .or_else(|| self.resource_symbol.clone())
            .unwrap_or_else(|| "the resource".to_string())
    }
}

fn row_to_request(r: &Row) -> rusqlite::Result<Request> {
    let body: String = r.get(5)?;
    let state: String = r.get(11)?;
    let response: Option<String> = r.get(16)?;
    Ok(Request {
        id: r.get(0)?,
        kind: r.get(1)?,
        from: r.get(2)?,
        to: r.get(3)?,
        subject: r.get(4)?,
        body: serde_json::from_str(&body).unwrap_or_else(|_| json!({})),
        resource_symbol: r.get(6)?,
        resource_handle: r.get(7)?,
        resource_task: r.get(8)?,
        task: r.get(9)?,
        priority: r.get(10)?,
        state: RequestState::parse(&state).unwrap_or(RequestState::Open),
        created_at: r.get(12)?,
        deadline_at: r.get(13)?,
        resolved_at: r.get(14)?,
        resolver: r.get(15)?,
        response: response.and_then(|s| serde_json::from_str(&s).ok()),
    })
}

fn row_to_progress(r: &Row) -> rusqlite::Result<ProgressUpdate> {
    Ok(ProgressUpdate {
        id: r.get(0)?,
        agent: r.get(1)?,
        task: r.get(2)?,
        symbol_id: r.get(3)?,
        symbol_handle: r.get(4)?,
        percent: r.get(5)?,
        eta_secs: r.get(6)?,
        note: r.get(7)?,
        ts: r.get(8)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lease::AcquireOptions;
    use crate::scan;

    fn fixture() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("src/pay.ts"),
            "export class PaymentService {\n  processPayment(id: string) { return 1; }\n  refund(id: string) { return 2; }\n}\nexport function audit() { return 3; }\n",
        )
        .unwrap();
        let mut store = Store::init(dir.path()).unwrap();
        scan::scan(&mut store, crate::ids::DEFAULT_REPO_ID, dir.path(), &[], false).unwrap();
        store.register_agent("agent-a", "claude").unwrap();
        store.register_agent("agent-b", "cursor").unwrap();
        (dir, store)
    }

    #[test]
    fn accepting_a_transfer_moves_the_lease_without_releasing_it() {
        let (_d, mut store) = fixture();
        let sym = store.resolve("PaymentService.processPayment").unwrap();
        let original = store
            .acquire(&sym.id, "agent-a", &AcquireOptions::default())
            .unwrap()
            .lease()
            .unwrap()
            .clone();

        let req = store
            .request_lease_transfer(&sym.id, "agent-b", Some("hotfix"), Some(300), 5, None)
            .unwrap();
        assert_eq!(req.to.as_deref(), Some("agent-a"), "routed to the holder");
        assert_eq!(req.body["current_task"], Value::Null);

        let resolved = store.accept_request(&req.id, "agent-a", None).unwrap();
        assert_eq!(resolved.state, RequestState::Fulfilled);

        let active = store.active_leases(None).unwrap();
        assert_eq!(active.len(), 1, "the symbol is never unowned mid-handoff");
        assert_eq!(active[0].agent, "agent-b");
        assert_eq!(active[0].id, original.id, "same lease, new owner");

        let kinds: Vec<String> = store
            .recent_events(30)
            .unwrap()
            .into_iter()
            .map(|e| e.kind)
            .collect();
        assert!(kinds.contains(&"lease.transferred".to_string()), "{kinds:?}");
    }

    #[test]
    fn declining_leaves_the_lease_where_it_was() {
        let (_d, mut store) = fixture();
        let sym = store.resolve("audit").unwrap();
        store
            .acquire(&sym.id, "agent-a", &AcquireOptions::default())
            .unwrap();
        let req = store
            .request_lease_transfer(&sym.id, "agent-b", None, Some(60), 0, None)
            .unwrap();

        let resolved = store
            .decline_request(&req.id, "agent-a", Some("mid-refactor, 90s"))
            .unwrap();
        assert_eq!(resolved.state, RequestState::Declined);
        assert_eq!(resolved.response.unwrap()["reason"], "mid-refactor, 90s");
        assert_eq!(store.active_leases(None).unwrap()[0].agent, "agent-a");
    }

    #[test]
    fn releasing_a_symbol_answers_whoever_asked_for_it() {
        let (_d, mut store) = fixture();
        let sym = store.resolve("audit").unwrap();
        store
            .acquire(&sym.id, "agent-a", &AcquireOptions::default())
            .unwrap();
        let req = store
            .request_lease_transfer(&sym.id, "agent-b", None, Some(300), 0, None)
            .unwrap();

        // agent-a just finishes and releases, never answering the request.
        store.release_all("agent-a").unwrap();

        let after = store.request(&req.id).unwrap().unwrap();
        assert_eq!(after.state, RequestState::Fulfilled);
        assert_eq!(after.response.unwrap()["symbol_free"], true);
    }

    #[test]
    fn a_dependency_clears_when_its_task_completes() {
        let (_d, mut store) = fixture();
        let task = store.add_task("payment provider interface", 5, &[]).unwrap();
        let req = store
            .open_request(&NewRequest {
                resource_task: Some(task.id.clone()),
                to: Some("agent-a".to_string()),
                body: json!({ "needs": ["authorize", "capture"] }),
                ..NewRequest::new(request_kind::DEPENDENCY, "agent-b", "blocked on provider")
            })
            .unwrap();

        assert_eq!(store.request(&req.id).unwrap().unwrap().state, RequestState::Open);
        store
            .set_task_state(&task.id, TaskState::Done, Some("agent-a"), None, false)
            .unwrap();

        let after = store.request(&req.id).unwrap().unwrap();
        assert_eq!(after.state, RequestState::Fulfilled);
        assert_eq!(after.response.unwrap()["task"], task.id);
    }

    #[test]
    fn interface_requests_are_accepted_then_fulfilled() {
        let (_d, mut store) = fixture();
        let req = store
            .open_request(&NewRequest {
                to: Some("agent-a".to_string()),
                body: json!({ "resource": "PaymentProvider", "methods": ["authorize", "capture"] }),
                deadline_secs: Some(300),
                ..NewRequest::new(request_kind::INTERFACE, "agent-b", "need PaymentProvider")
            })
            .unwrap();

        let accepted = store.accept_request(&req.id, "agent-a", None).unwrap();
        assert_eq!(accepted.state, RequestState::Accepted);
        assert!(accepted.state.is_live(), "still waiting on delivery");

        let done = store
            .fulfill_request(
                &req.id,
                "agent-a",
                Some(json!({ "version": 2, "breaking_changes": false })),
            )
            .unwrap();
        assert_eq!(done.state, RequestState::Fulfilled);
        assert_eq!(done.response.unwrap()["version"], 2);
    }

    #[test]
    fn only_the_addressee_may_answer() {
        let (_d, mut store) = fixture();
        let req = store
            .open_request(&NewRequest {
                to: Some("agent-a".to_string()),
                ..NewRequest::new(request_kind::QUESTION, "agent-b", "ping")
            })
            .unwrap();
        assert!(store.accept_request(&req.id, "agent-c", None).is_err());
        assert!(
            store.accept_request(&req.id, "agent-b", None).is_err(),
            "sender cannot self-answer"
        );
        assert!(store.accept_request(&req.id, "agent-a", None).is_ok());
        assert!(
            store.accept_request(&req.id, "agent-a", None).is_err(),
            "committing twice is a bug on the responder's side"
        );
        // Backing out after accepting is legal: agents hit walls.
        let bailed = store
            .decline_request(&req.id, "agent-a", Some("cannot do it after all"))
            .unwrap();
        assert_eq!(bailed.state, RequestState::Declined);
        assert!(store.fulfill_request(&req.id, "agent-a", None).is_err());
    }

    #[test]
    fn broadcasts_can_be_answered_by_anyone_but_the_sender() {
        let (_d, mut store) = fixture();
        let req = store
            .open_request(&NewRequest {
                to: None,
                ..NewRequest::new(request_kind::QUESTION, "agent-b", "who owns the schema?")
            })
            .unwrap();
        let inbox = store.requests(Some("agent-a"), Direction::Inbox, true).unwrap();
        assert_eq!(inbox.len(), 1, "broadcasts land in everyone's inbox");
        assert!(store
            .requests(Some("agent-b"), Direction::Inbox, true)
            .unwrap()
            .is_empty());
        assert!(store.fulfill_request(&req.id, "agent-a", None).is_ok());
    }

    #[test]
    fn unanswered_requests_expire_on_their_deadline() {
        let (_d, mut store) = fixture();
        let req = store
            .open_request(&NewRequest {
                to: Some("agent-a".to_string()),
                deadline_secs: Some(0),
                ..NewRequest::new(request_kind::QUESTION, "agent-b", "urgent")
            })
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        assert_eq!(store.expire_requests().unwrap(), 1);

        let after = store.request(&req.id).unwrap().unwrap();
        assert_eq!(after.state, RequestState::Expired);
        assert!(!after.state.is_live(), "a waiting agent stops waiting");
        assert!(store.accept_request(&req.id, "agent-a", None).is_err());
    }

    #[test]
    fn transfer_requires_still_holding_the_lease() {
        let (_d, mut store) = fixture();
        let sym = store.resolve("audit").unwrap();
        store
            .acquire(&sym.id, "agent-a", &AcquireOptions::default())
            .unwrap();
        let req = store
            .request_lease_transfer(&sym.id, "agent-b", None, Some(300), 0, None)
            .unwrap();

        // A third agent takes over first; agent-a can no longer hand it on.
        store
            .acquire(
                &sym.id,
                "agent-c",
                &AcquireOptions {
                    priority: 9,
                    preempt: true,
                    ..Default::default()
                },
            )
            .unwrap();
        let err = store.accept_request(&req.id, "agent-a", None).unwrap_err();
        assert!(err.to_string().contains("no longer holds"), "{err}");
        assert_eq!(store.active_leases(None).unwrap()[0].agent, "agent-c");
    }

    #[test]
    fn progress_updates_report_and_heartbeat() {
        let (_d, mut store) = fixture();
        let sym = store.resolve("audit").unwrap();
        let lease = store
            .acquire(
                &sym.id,
                "agent-a",
                &AcquireOptions {
                    ttl_secs: 60,
                    ..Default::default()
                },
            )
            .unwrap()
            .lease()
            .unwrap()
            .clone();
        std::thread::sleep(std::time::Duration::from_millis(20));

        let p = store
            .record_progress(
                "agent-a",
                Some("T1"),
                Some(&sym.id),
                Some(60),
                Some(120),
                Some("authorize() done"),
            )
            .unwrap();
        assert_eq!(p.percent, Some(60));
        assert_eq!(p.symbol_handle.as_deref(), Some("src/pay.ts:audit"));

        let fresh = store.lease(&lease.id).unwrap().unwrap();
        assert!(fresh.expires_at > lease.expires_at, "progress is a heartbeat");

        let latest = store.latest_progress().unwrap();
        assert_eq!(latest.len(), 1);
        assert_eq!(latest[0].note.as_deref(), Some("authorize() done"));
    }

    #[test]
    fn percent_is_clamped_to_something_meaningful() {
        let (_d, mut store) = fixture();
        let p = store
            .record_progress("agent-a", None, None, Some(430), None, None)
            .unwrap();
        assert_eq!(p.percent, Some(100));
    }

    #[test]
    fn asking_for_an_unheld_symbol_is_an_error_not_a_request() {
        let (_d, mut store) = fixture();
        let sym = store.resolve("audit").unwrap();
        let err = store
            .request_lease_transfer(&sym.id, "agent-b", None, None, 0, None)
            .unwrap_err();
        assert!(err.to_string().contains("just acquire it"), "{err}");
    }
}
