//! The tool surface.
//!
//! Names are bare — a client namespaces them itself (Claude Code exposes these
//! as `mcp__atlas__next_task`), so a `atlas_` prefix would stutter.
//!
//! Descriptions and schemas here *are* the API the model programs against, so
//! they are written as literals rather than derived: what a field means to a
//! model is not something a `#[derive]` can express.
//!
//! Note what is **not** here: registering, heartbeating, renewing leases and
//! leaving. Those happen whether or not the model ever calls anything, which
//! is the difference between an adapter and an instruction sheet.

use anyhow::{anyhow, bail, Result};
use atlas_core::activity::{self, NewActivity};
use atlas_core::lease::{AcquireOptions, DEFAULT_TTL_SECS};
use atlas_core::model::*;
use atlas_core::protocol::{Direction, NewRequest};
use atlas_core::workspace;
use serde_json::{json, Value};

use crate::server::{arg_bool, arg_i64, arg_str, require_str, Session, ToolOutcome};

pub struct Tool {
    pub name: &'static str,
    pub description: &'static str,
    pub schema: fn() -> Value,
    pub handler: fn(&mut Session, &Value) -> Result<ToolOutcome>,
}

pub fn all() -> Vec<Tool> {
    vec![
        Tool {
            name: "whoami",
            description: "Who you are in this workspace, what you are holding, and what task you \
                          are on. Cheapest way to pick up anything waiting for you.",
            schema: || json!({ "type": "object", "properties": {}, "additionalProperties": false }),
            handler: whoami,
        },
        Tool {
            name: "next_task",
            description: "Ask for work. Claims the highest-priority task you can safely start and \
                          leases its scope in the same transaction. If you are already on a task \
                          this renews it instead of taking another. Returns task: null when there \
                          is nothing to do — that is a normal answer, not a failure.",
            schema: || {
                json!({
                    "type": "object",
                    "properties": {
                        "goal": { "type": "string", "description": "Only claim work under this goal id, e.g. G1." },
                        "ttl_secs": {
                            "type": "integer", "minimum": 30, "default": DEFAULT_TTL_SECS,
                            "description": "How long to hold this task's scope. It is renewed for you automatically."
                        }
                    },
                    "additionalProperties": false
                })
            },
            handler: next_task,
        },
        Tool {
            name: "task_context",
            description: "Everything worth knowing before you start: the symbols in scope and who \
                          holds them, what calls them, the tests that cover them, decisions the \
                          team already made, and who is working next door. Call this instead of \
                          exploring the repository yourself. The `context` field says which shape \
                          came back: 'task' when you are working, 'agent' (goals and what you \
                          could start) when you are idle.",
            schema: || {
                json!({
                    "type": "object",
                    "properties": {
                        "task": { "type": "string", "description": "Task id. Defaults to your current task." },
                        "depth": { "type": "integer", "minimum": 1, "maximum": 4, "default": 2,
                                   "description": "How many hops of blast radius to include." }
                    },
                    "additionalProperties": false
                })
            },
            handler: task_context,
        },
        Tool {
            name: "check_edit",
            description: "May you edit this file right now? Call this BEFORE editing. \
                          verdict 'allowed' means a lease of yours covers it; 'warn' means nobody \
                          holds it and you may proceed unprotected; 'denied' means somebody else \
                          holds it — do not edit, and use the suggestions to ask for it or to \
                          narrow to a symbol they do not hold.",
            schema: || {
                json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "Repo-relative or absolute file you are about to edit." },
                        "symbol": { "type": "string", "description": "Narrow to one symbol: id, path:Fqn, fqn or bare name." }
                    },
                    "required": ["path"],
                    "additionalProperties": false
                })
            },
            handler: check_edit,
        },
        Tool {
            name: "claim_symbol",
            description: "Take a lease on something outside your current task's scope. Denial is \
                          a normal answer and tells you who holds it.",
            schema: || {
                json!({
                    "type": "object",
                    "properties": {
                        "symbol": { "type": "string" },
                        "ttl_secs": { "type": "integer", "default": DEFAULT_TTL_SECS },
                        "note": { "type": "string", "description": "Why, for whoever else wants it." },
                        "queue": { "type": "boolean", "default": true,
                                   "description": "Take a place in line if it is held right now." }
                    },
                    "required": ["symbol"],
                    "additionalProperties": false
                })
            },
            handler: claim_symbol,
        },
        Tool {
            name: "release_symbol",
            description: "Hand a symbol back before its lease runs out, so whoever is waiting can \
                          have it.",
            schema: || {
                json!({
                    "type": "object",
                    "properties": { "symbol": { "type": "string" } },
                    "required": ["symbol"],
                    "additionalProperties": false
                })
            },
            handler: release_symbol,
        },
        Tool {
            name: "progress",
            description: "Say what you are doing. Visible to every human and agent in the \
                          workspace, and it renews your leases as a side effect.",
            schema: || {
                json!({
                    "type": "object",
                    "properties": {
                        "note": { "type": "string" },
                        "percent": { "type": "integer", "minimum": 0, "maximum": 100 },
                        "eta_secs": { "type": "integer" },
                        "symbol": { "type": "string", "description": "What you are working on right now." },
                        "task": { "type": "string" }
                    },
                    "additionalProperties": false
                })
            },
            handler: progress,
        },
        Tool {
            name: "submit_work",
            description: "Declare a task finished. 'review' (the default) keeps your leases until \
                          a human approves; 'done' releases them and unblocks whatever was \
                          waiting on you.",
            schema: || {
                json!({
                    "type": "object",
                    "properties": {
                        "task": { "type": "string", "description": "Defaults to your current task." },
                        "note": { "type": "string" },
                        "state": { "type": "string", "enum": ["review", "done"], "default": "review" }
                    },
                    "additionalProperties": false
                })
            },
            handler: submit_work,
        },
        Tool {
            name: "report_failure",
            description: "Say you cannot finish. If you are stuck behind somebody else's lease, \
                          name it in blocked_by_symbol and a handover request is opened for you in \
                          the same call.",
            schema: || {
                json!({
                    "type": "object",
                    "properties": {
                        "reason": { "type": "string" },
                        "task": { "type": "string", "description": "Defaults to your current task." },
                        "blocked_by_symbol": { "type": "string" },
                        "permanent": { "type": "boolean", "default": false,
                                       "description": "true fails the task outright; false (default) marks it blocked so it can resume." }
                    },
                    "required": ["reason"],
                    "additionalProperties": false
                })
            },
            handler: report_failure,
        },
        Tool {
            name: "inbox",
            description: "Structured asks addressed to you from other agents and from the runtime \
                          — handover requests, interface requests, review comments, notices that \
                          an API you depend on changed. Answer them with `respond`.",
            schema: || {
                json!({
                    "type": "object",
                    "properties": { "all": { "type": "boolean", "default": false,
                                             "description": "Include already-resolved requests." } },
                    "additionalProperties": false
                })
            },
            handler: inbox,
        },
        Tool {
            name: "respond",
            description: "Answer a request. Accepting a lease-transfer performs the handover \
                          atomically — the symbol is theirs the moment you accept.",
            schema: || {
                json!({
                    "type": "object",
                    "properties": {
                        "request": { "type": "string" },
                        "action": { "type": "string", "enum": ["accept", "decline", "fulfill", "cancel"] },
                        "reason": { "type": "string", "description": "Why, when declining." },
                        "body": { "type": "object", "description": "Structured answer, e.g. the interface you delivered." }
                    },
                    "required": ["request", "action"],
                    "additionalProperties": false
                })
            },
            handler: respond,
        },
        Tool {
            name: "ask",
            description: "Open a structured request to another agent, or to the whole workspace \
                          when `to` is omitted. Use kind 'lease-transfer' to ask for a symbol \
                          somebody holds, 'interface' to ask for something to exist, 'question' \
                          for anything else.",
            schema: || {
                json!({
                    "type": "object",
                    "properties": {
                        "kind": { "type": "string",
                                  "enum": ["lease-transfer", "interface", "dependency", "review", "question"],
                                  "default": "question" },
                        "subject": { "type": "string" },
                        "to": { "type": "string", "description": "Omit to ask everyone." },
                        "symbol": { "type": "string", "description": "Required for lease-transfer: what you want." },
                        "reason": { "type": "string" },
                        "deadline_secs": { "type": "integer", "default": 300 },
                        "priority": { "type": "integer", "default": 0 }
                    },
                    "additionalProperties": false
                })
            },
            handler: ask,
        },
        Tool {
            name: "wait_for",
            description: "Block until a request you opened is answered, then return the answer. \
                          Use this after `ask` instead of polling. Returns state 'open' if it is \
                          still unanswered when the wait runs out — call again or move on.",
            schema: || {
                json!({
                    "type": "object",
                    "properties": {
                        "request": { "type": "string" },
                        "max_secs": { "type": "integer", "minimum": 1, "maximum": 25, "default": 15 }
                    },
                    "required": ["request"],
                    "additionalProperties": false
                })
            },
            handler: wait_for,
        },
        Tool {
            name: "impact",
            description: "What else moves if you change this, and who owns it. Ask before a \
                          change, not after.",
            schema: || {
                json!({
                    "type": "object",
                    "properties": {
                        "symbol": { "type": "string" },
                        "depth": { "type": "integer", "minimum": 1, "maximum": 4, "default": 2 }
                    },
                    "required": ["symbol"],
                    "additionalProperties": false
                })
            },
            handler: impact,
        },
        Tool {
            name: "who_owns",
            description: "Who currently holds a symbol, and when it frees up.",
            schema: || {
                json!({
                    "type": "object",
                    "properties": { "symbol": { "type": "string" } },
                    "required": ["symbol"],
                    "additionalProperties": false
                })
            },
            handler: who_owns,
        },
        Tool {
            name: "note",
            description: "Shared project memory. Write down a decision or a convention once so \
                          nobody rediscovers it; read what the team already established. Tag \
                          entries 'architecture', 'convention', 'decision' or 'interface' and they \
                          are handed to every agent that touches related code.",
            schema: || {
                json!({
                    "type": "object",
                    "properties": {
                        "key": { "type": "string" },
                        "value": { "type": "string", "description": "Omit to read instead of write." },
                        "tags": { "type": "array", "items": { "type": "string" } }
                    },
                    "additionalProperties": false
                })
            },
            handler: note,
        },
        Tool {
            name: "find_symbols",
            description: "Search the indexed symbol graph by name, path or kind. Cheaper and more \
                          precise than grepping, and the results are leasable references.",
            schema: || {
                json!({
                    "type": "object",
                    "properties": {
                        "pattern": { "type": "string" },
                        "path": { "type": "string" },
                        "kind": { "type": "string",
                                  "enum": ["function", "method", "class", "module", "file", "service", "table"] },
                        "limit": { "type": "integer", "default": 40, "maximum": 200 }
                    },
                    "additionalProperties": false
                })
            },
            handler: find_symbols,
        },
    ]
}

pub fn find(name: &str) -> Option<Tool> {
    all().into_iter().find(|t| t.name == name)
}

pub fn list() -> Value {
    let tools: Vec<Value> = all()
        .iter()
        .map(|t| {
            json!({
                "name": t.name,
                "description": t.description,
                "inputSchema": (t.schema)(),
            })
        })
        .collect();
    json!({ "tools": tools })
}

// ------------------------------------------------------------------ handlers

fn whoami(s: &mut Session, _args: &Value) -> Result<ToolOutcome> {
    let view = s
        .store
        .agents()?
        .into_iter()
        .find(|a| a.agent.name == s.agent)
        .ok_or_else(|| anyhow!("{} is not registered", s.agent))?;
    let leases = s.store.active_leases(Some(&s.agent))?;
    let sessions = s.store.sessions_for(&s.agent)?;

    let text = format!(
        "You are {} ({} via MCP). {}{}",
        s.agent,
        s.tool,
        match &view.current_task {
            Some(t) => format!("Working on {t}."),
            None => "No task claimed — call next_task.".to_string(),
        },
        if leases.is_empty() {
            String::new()
        } else {
            format!(" Holding {} symbol(s).", leases.len())
        }
    );
    Ok(ToolOutcome::new(
        text,
        json!({ "agent": view, "leases": leases, "sessions": sessions }),
    ))
}

fn next_task(s: &mut Session, args: &Value) -> Result<ToolOutcome> {
    let ttl = arg_i64(args, "ttl_secs").unwrap_or(DEFAULT_TTL_SECS);
    let goal = arg_str(args, "goal").map(|g| g.to_string());

    // Mirrors `atlas continue` exactly, including the order: already working
    // means renew, never reach for a second task.
    let current = s
        .store
        .agents()?
        .into_iter()
        .find(|a| a.agent.name == s.agent)
        .and_then(|a| a.current_task);

    if let Some(task) = current {
        let renewed = s.store.heartbeat(&s.agent, None)?;
        return Ok(ToolOutcome::new(
            format!("Still on {task}; {} lease(s) renewed.", renewed.len()),
            json!({ "task": task, "renewed": renewed, "claimed": false }),
        ));
    }

    match s.store.claim_next_in(&s.agent, ttl, goal.as_deref())? {
        Some(t) => {
            let scope: Vec<String> = t.scope.iter().map(|x| x.handle()).collect();
            Ok(ToolOutcome::new(
                format!(
                    "Claimed {}: {}. You now hold {}. Call task_context for orientation.",
                    t.task.task.id,
                    t.task.task.title,
                    if scope.is_empty() {
                        "no symbols (unscoped task)".to_string()
                    } else {
                        scope.join(", ")
                    }
                ),
                json!({ "task": t, "claimed": true }),
            ))
        }
        // Not an error: a legitimate "nothing right now", which the model must
        // be able to branch on.
        None => Ok(ToolOutcome::new(
            "Nothing startable right now.".to_string(),
            json!({ "task": null, "claimed": false }),
        )),
    }
}

fn task_context(s: &mut Session, args: &Value) -> Result<ToolOutcome> {
    let depth = arg_i64(args, "depth").unwrap_or(2).clamp(1, 4) as usize;
    let task = match arg_str(args, "task") {
        Some(t) => Some(t.to_string()),
        None => s
            .store
            .agents()?
            .into_iter()
            .find(|a| a.agent.name == s.agent)
            .and_then(|a| a.current_task),
    };

    // Two genuinely different questions — "what is this task" and "what could
    // I pick up" — so the payload differs. `context` says which one came back,
    // rather than making the model infer it from which keys are present.
    let (text, mut data) = match task {
        Some(id) => {
            let ctx = s.store.task_context(&id, depth)?;
            let text = format!(
                "{}: {}. Scope: {}. {} symbol(s) downstream, {} test(s) cover it{}.",
                ctx.task.task.id,
                ctx.task.task.title,
                ctx.scope
                    .iter()
                    .map(|x| x.symbol.handle())
                    .collect::<Vec<_>>()
                    .join(", "),
                ctx.impact.len(),
                ctx.tests.len(),
                if ctx.neighbors_at_work.is_empty() {
                    String::new()
                } else {
                    format!(
                        "; {} also working nearby",
                        ctx.neighbors_at_work
                            .iter()
                            .map(|a| a.agent.name.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                }
            );
            (text, serde_json::to_value(ctx)?)
        }
        None => {
            let ctx = s.store.agent_context(&s.agent, depth)?;
            let text = format!(
                "No task claimed. {} startable, {} open goal(s). Call next_task to be given one.",
                ctx.startable.len(),
                ctx.open_goals.len()
            );
            (text, serde_json::to_value(ctx)?)
        }
    };
    if let Some(obj) = data.as_object_mut() {
        let kind = if obj.contains_key("scope") { "task" } else { "agent" };
        obj.insert("context".to_string(), json!(kind));
    }
    Ok(ToolOutcome::new(text, data))
}

fn check_edit(s: &mut Session, args: &Value) -> Result<ToolOutcome> {
    let path = require_str(args, "path")?;
    let report = workspace::guard_workspace(
        &s.store,
        &s.root,
        &s.agent,
        std::path::Path::new(path),
        arg_str(args, "symbol"),
    )?;

    let text = match report.verdict {
        atlas_core::guard::GuardVerdict::Denied => format!(
            "DO NOT EDIT. {}. {}",
            report.summary,
            report
                .suggestions
                .first()
                .map(|g| format!("Suggested: {} ({})", g.action, g.tool))
                .unwrap_or_default()
        ),
        _ => report.summary.clone(),
    };
    // A tool that asks permission is a tool that is about to edit. Recording
    // it here gives a model that speaks MCP but runs no editor hooks the same
    // presence as one that does.
    note_activity(
        s,
        &report.repo_id,
        &report.path,
        if report.blocking() {
            activity::kind::BLOCKED
        } else {
            activity::kind::EDITING
        },
        report.anchor.clone(),
        report.anchor_handle.clone(),
    );

    // Denied is still isError: false — the model has to branch on it, not
    // treat it as a fault.
    Ok(ToolOutcome::new(text, serde_json::to_value(report)?))
}

/// Best-effort: an activity write must never turn a working tool call into a
/// failed one. The window closes on its own if this silently does nothing.
fn note_activity(
    s: &mut Session,
    repo_id: &str,
    path: &str,
    kind: &str,
    symbol_id: Option<String>,
    symbol_handle: Option<String>,
) {
    let session_id = s.session_id.clone();
    let agent = s.agent.clone();
    s.store
        .record_activity(&NewActivity {
            session_id,
            symbol_id,
            symbol_handle,
            ..NewActivity::new(&agent, repo_id, path, kind)
        })
        .ok();
}

fn claim_symbol(s: &mut Session, args: &Value) -> Result<ToolOutcome> {
    let symbol = require_str(args, "symbol")?;
    let opts = AcquireOptions {
        ttl_secs: arg_i64(args, "ttl_secs").unwrap_or(DEFAULT_TTL_SECS),
        note: arg_str(args, "note").map(|n| n.to_string()),
        queue: arg_bool(args, "queue", true),
        ..Default::default()
    };
    let (sym, outcome) = s.store.acquire_ref(symbol, &s.agent, &opts)?;

    // Claiming a symbol is the clearest statement of intent there is.
    if matches!(
        outcome,
        AcquireOutcome::Granted { .. } | AcquireOutcome::Extended { .. }
    ) {
        note_activity(
            s,
            &sym.repo_id,
            &sym.path,
            activity::kind::EDITING,
            Some(sym.id.clone()),
            Some(sym.handle()),
        );
    }

    let text = match &outcome {
        AcquireOutcome::Granted { .. } => format!("You now hold {}.", sym.handle()),
        AcquireOutcome::Extended { .. } => format!("Extended your lease on {}.", sym.handle()),
        AcquireOutcome::Denied { conflicts } => match conflicts.first() {
            Some(c) => format!(
                "Denied: {} is held by {} for another {}s. Use `ask` with kind lease-transfer.",
                sym.handle(),
                c.holder,
                c.seconds_until_free.max(0)
            ),
            None => format!("Denied: {} is not available.", sym.handle()),
        },
    };
    Ok(ToolOutcome::new(
        text,
        json!({ "symbol": sym, "result": outcome }),
    ))
}

fn release_symbol(s: &mut Session, args: &Value) -> Result<ToolOutcome> {
    let query = require_str(args, "symbol")?;
    let sym = s.store.resolve(query)?;
    let lease = s
        .store
        .active_leases(Some(&s.agent))?
        .into_iter()
        .find(|l| l.symbol_id == sym.id)
        .ok_or_else(|| anyhow!("you hold no lease on {}", sym.handle()))?;
    let released = s.store.release(&lease.id, &s.agent)?;
    Ok(ToolOutcome::new(
        format!("Released {}.", sym.handle()),
        json!({ "lease": released }),
    ))
}

fn progress(s: &mut Session, args: &Value) -> Result<ToolOutcome> {
    let symbol = match arg_str(args, "symbol") {
        Some(q) => Some(s.store.resolve(q)?),
        None => None,
    };
    let symbol_id = symbol.as_ref().map(|sym| sym.id.clone());
    let task = match arg_str(args, "task") {
        Some(t) => Some(t.to_string()),
        None => s
            .store
            .agents()?
            .into_iter()
            .find(|a| a.agent.name == s.agent)
            .and_then(|a| a.current_task),
    };
    let update = s.store.record_progress(
        &s.agent,
        task.as_deref(),
        symbol_id.as_deref(),
        arg_i64(args, "percent"),
        arg_i64(args, "eta_secs"),
        arg_str(args, "note"),
    )?;

    // Reporting progress on a named symbol is a live edit in flight — the same
    // statement a PostToolUse hook makes, from a tool that has no hooks.
    if let Some(sym) = &symbol {
        note_activity(
            s,
            &sym.repo_id,
            &sym.path,
            activity::kind::EDITED,
            Some(sym.id.clone()),
            Some(sym.handle()),
        );
    }

    Ok(ToolOutcome::new(
        "Progress published; your leases were renewed.".to_string(),
        serde_json::to_value(update)?,
    ))
}

fn submit_work(s: &mut Session, args: &Value) -> Result<ToolOutcome> {
    let task = current_task(s, args)?;
    let state = arg_str(args, "state").unwrap_or("review");
    let note = arg_str(args, "note");

    let (view, text) = match state {
        "done" => (
            s.store
                .set_task_state(&task, TaskState::Done, Some(&s.agent), note, false)?,
            format!("{task} is done; its leases are released."),
        ),
        "review" => (
            s.store.submit_for_review(&task, &s.agent)?,
            format!("{task} is up for review. You keep its leases until somebody approves."),
        ),
        other => bail!("state must be 'review' or 'done', not '{other}'"),
    };
    Ok(ToolOutcome::new(text, serde_json::to_value(view)?))
}

fn report_failure(s: &mut Session, args: &Value) -> Result<ToolOutcome> {
    let reason = require_str(args, "reason")?;
    let task = current_task(s, args)?;
    let permanent = arg_bool(args, "permanent", false);

    let state = if permanent {
        TaskState::Failed
    } else {
        TaskState::Blocked
    };
    let view = s
        .store
        .set_task_state(&task, state, Some(&s.agent), Some(reason), true)?;

    // "I'm stuck behind someone" and "here is a request asking them to move"
    // are the same thought; making it one call is what stops a blocked agent
    // from simply stopping.
    let opened = match arg_str(args, "blocked_by_symbol") {
        Some(q) => {
            let sym = s.store.resolve(q)?;
            Some(s.store.request_lease_transfer(
                &sym.id,
                &s.agent,
                Some(reason),
                Some(300),
                5,
                Some(&task),
            )?)
        }
        None => None,
    };

    let text = match &opened {
        Some(r) => format!("{task} marked {}. Asked {} for the symbol (request {}).",
            state.as_str(),
            r.to.as_deref().unwrap_or("the workspace"),
            r.id),
        None => format!("{task} marked {}.", state.as_str()),
    };
    Ok(ToolOutcome::new(
        text,
        json!({ "task": view, "request": opened }),
    ))
}

fn inbox(s: &mut Session, args: &Value) -> Result<ToolOutcome> {
    let live_only = !arg_bool(args, "all", false);
    let requests = s
        .store
        .requests(Some(&s.agent), Direction::Inbox, live_only)?;
    let text = if requests.is_empty() {
        "Nothing waiting for you.".to_string()
    } else {
        format!(
            "{} request(s) for you: {}",
            requests.len(),
            requests
                .iter()
                .take(5)
                .map(|r| format!("{} [{}] {}", r.id, r.kind, r.subject))
                .collect::<Vec<_>>()
                .join("; ")
        )
    };
    Ok(ToolOutcome::new(text, serde_json::to_value(requests)?))
}

fn respond(s: &mut Session, args: &Value) -> Result<ToolOutcome> {
    let id = require_str(args, "request")?;
    let action = require_str(args, "action")?;
    let body = args.get("body").cloned();
    let reason = arg_str(args, "reason");

    let answered = match action {
        "accept" => s.store.accept_request(id, &s.agent, body)?,
        "decline" => s.store.decline_request(id, &s.agent, reason)?,
        "fulfill" => s.store.fulfill_request(id, &s.agent, body)?,
        "cancel" => s.store.cancel_request(id, &s.agent)?,
        other => bail!("action must be accept, decline, fulfill or cancel, not '{other}'"),
    };

    let text = if answered.kind == request_kind::LEASE_TRANSFER && action == "accept" {
        format!(
            "Handed {} to {}. It is theirs now.",
            answered.resource_handle.as_deref().unwrap_or("the symbol"),
            answered.from
        )
    } else {
        format!("{id} {}.", answered.state.as_str())
    };
    Ok(ToolOutcome::new(text, serde_json::to_value(answered)?))
}

fn ask(s: &mut Session, args: &Value) -> Result<ToolOutcome> {
    let kind = arg_str(args, "kind").unwrap_or(request_kind::QUESTION);
    let deadline = arg_i64(args, "deadline_secs").unwrap_or(300);
    let priority = arg_i64(args, "priority").unwrap_or(0);
    let reason = arg_str(args, "reason");

    // The runtime knows who holds a symbol, so a handover request should never
    // need the asker to work out the addressee.
    if kind == request_kind::LEASE_TRANSFER {
        let query = arg_str(args, "symbol")
            .ok_or_else(|| anyhow!("a lease-transfer needs `symbol`: what do you want?"))?;
        let sym = s.store.resolve(query)?;
        let task = s
            .store
            .agents()?
            .into_iter()
            .find(|a| a.agent.name == s.agent)
            .and_then(|a| a.current_task);
        let opened = s.store.request_lease_transfer(
            &sym.id,
            &s.agent,
            reason,
            Some(deadline),
            priority,
            task.as_deref(),
        )?;
        return Ok(ToolOutcome::new(
            format!(
                "Asked {} for {}. Poll `inbox` or call whoami to see the answer.",
                opened.to.as_deref().unwrap_or("whoever holds it"),
                sym.handle()
            ),
            serde_json::to_value(opened)?,
        ));
    }

    let symbol_id = match arg_str(args, "symbol") {
        Some(q) => Some(s.store.resolve(q)?.id),
        None => None,
    };
    let subject = arg_str(args, "subject")
        .map(|x| x.to_string())
        .or_else(|| reason.map(|r| r.to_string()))
        .ok_or_else(|| anyhow!("say what you are asking for: pass `subject`"))?;

    let opened = s.store.open_request(&NewRequest {
        to: arg_str(args, "to").map(|t| t.to_string()),
        body: json!({ "reason": reason }),
        resource_symbol: symbol_id,
        deadline_secs: Some(deadline),
        priority,
        ..NewRequest::new(kind, &s.agent, &subject)
    })?;
    Ok(ToolOutcome::new(
        format!(
            "Opened {} to {}.",
            opened.id,
            opened.to.as_deref().unwrap_or("the whole workspace")
        ),
        serde_json::to_value(opened)?,
    ))
}

fn wait_for(s: &mut Session, args: &Value) -> Result<ToolOutcome> {
    let id = require_str(args, "request")?;
    // Clamped hard: a tool call that outlives the client's own timeout looks
    // like a hung server, which is worse than returning "still open".
    let max = arg_i64(args, "max_secs").unwrap_or(15).clamp(1, 25);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(max as u64);

    loop {
        s.store.expire_requests().ok();
        let req = s
            .store
            .request(id)?
            .ok_or_else(|| anyhow!("no such request: {id}"))?;

        if !req.state.is_live() {
            let text = match req.state {
                RequestState::Fulfilled => format!(
                    "{id} was fulfilled by {}.",
                    req.resolver.as_deref().unwrap_or("someone")
                ),
                RequestState::Declined => format!(
                    "{id} was declined{}.",
                    req.response
                        .as_ref()
                        .and_then(|b| b.get("reason"))
                        .and_then(|r| r.as_str())
                        .map(|r| format!(": {r}"))
                        .unwrap_or_default()
                ),
                other => format!("{id} is {}.", other.as_str()),
            };
            return Ok(ToolOutcome::new(text, serde_json::to_value(req)?));
        }
        if std::time::Instant::now() >= deadline {
            return Ok(ToolOutcome::new(
                format!("{id} is still unanswered after {max}s."),
                serde_json::to_value(req)?,
            ));
        }
        std::thread::sleep(std::time::Duration::from_millis(400));
    }
}

fn impact(s: &mut Session, args: &Value) -> Result<ToolOutcome> {
    let depth = arg_i64(args, "depth").unwrap_or(2).clamp(1, 4) as usize;
    let sym = s.store.resolve(require_str(args, "symbol")?)?;
    let nodes = s.store.impact(&sym.id, depth)?;
    let neighbors = s.store.neighbors(&sym.id)?;

    let held: Vec<String> = nodes
        .iter()
        .filter_map(|n| {
            n.lease
                .as_ref()
                .map(|l| format!("{} ({})", n.symbol.handle(), l.agent))
        })
        .collect();
    let text = format!(
        "Changing {} reaches {} symbol(s){}.",
        sym.handle(),
        nodes.len(),
        if held.is_empty() {
            String::new()
        } else {
            format!("; held by others: {}", held.join(", "))
        }
    );
    Ok(ToolOutcome::new(
        text,
        json!({ "symbol": sym, "impact": nodes, "neighbors": neighbors }),
    ))
}

fn who_owns(s: &mut Session, args: &Value) -> Result<ToolOutcome> {
    let sym = s.store.resolve(require_str(args, "symbol")?)?;
    let conflicts = s.store.conflicts_for(&sym.id, &s.agent)?;
    let mine = s
        .store
        .active_leases(Some(&s.agent))?
        .into_iter()
        .find(|l| l.symbol_id == sym.id);

    let text = match (mine.is_some(), conflicts.first()) {
        (true, _) => format!("{} is yours.", sym.handle()),
        (false, Some(c)) => format!(
            "{} is held by {} for another {}s.",
            sym.handle(),
            c.holder,
            c.seconds_until_free.max(0)
        ),
        (false, None) => format!("Nobody holds {}.", sym.handle()),
    };
    Ok(ToolOutcome::new(
        text,
        json!({ "symbol": sym, "yours": mine, "conflicts": conflicts }),
    ))
}

fn note(s: &mut Session, args: &Value) -> Result<ToolOutcome> {
    let tags: Vec<String> = args
        .get("tags")
        .and_then(|t| t.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();

    match (arg_str(args, "key"), arg_str(args, "value")) {
        (Some(key), Some(value)) => {
            s.store.memory_set(key, value, Some(&s.agent), &tags)?;
            Ok(ToolOutcome::new(
                format!("Noted '{key}' for everyone."),
                serde_json::to_value(s.store.memory_get(key)?)?,
            ))
        }
        (Some(key), None) => match s.store.memory_get(key)? {
            Some(entry) => Ok(ToolOutcome::new(
                entry.value.clone(),
                serde_json::to_value(entry)?,
            )),
            None => Ok(ToolOutcome::new(
                format!("Nothing recorded under '{key}'."),
                json!(null),
            )),
        },
        (None, _) => {
            let entries = s.store.memory_list(tags.first().map(|t| t.as_str()))?;
            let text = if entries.is_empty() {
                "Nothing recorded yet.".to_string()
            } else {
                entries
                    .iter()
                    .map(|m| format!("{}: {}", m.key, m.value))
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            Ok(ToolOutcome::new(text, serde_json::to_value(entries)?))
        }
    }
}

fn find_symbols(s: &mut Session, args: &Value) -> Result<ToolOutcome> {
    let limit = arg_i64(args, "limit").unwrap_or(40).clamp(1, 200) as usize;
    let kind = arg_str(args, "kind").and_then(SymbolKind::parse);
    let found = s
        .store
        .list_symbols(arg_str(args, "path"), kind, arg_str(args, "pattern"), limit)?;
    let text = if found.is_empty() {
        "No symbols matched. The index may be stale.".to_string()
    } else {
        found
            .iter()
            .take(30)
            .map(|x| format!("{} [{}]", x.handle(), x.kind.as_str()))
            .collect::<Vec<_>>()
            .join("\n")
    };
    Ok(ToolOutcome::new(text, serde_json::to_value(found)?))
}

/// The task named in `args`, or the one this agent is already on.
fn current_task(s: &mut Session, args: &Value) -> Result<String> {
    if let Some(t) = arg_str(args, "task") {
        return Ok(t.to_string());
    }
    s.store
        .agents()?
        .into_iter()
        .find(|a| a.agent.name == s.agent)
        .and_then(|a| a.current_task)
        .ok_or_else(|| anyhow!("you have no current task — pass `task`, or call next_task first"))
}
