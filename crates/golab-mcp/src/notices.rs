//! Telling an agent something it did not ask about.
//!
//! The requirement is "event-driven, not command-driven": an agent should not
//! have to poll to learn that someone wants its lease, that its dependency
//! landed, or that the API under it moved. MCP gives no mechanism for that.
//! There is no way for a server to inject a turn, and clients almost never
//! surface `notifications/message` to the model — so a notification channel
//! that only pushes is a channel that reaches nobody.
//!
//! What *is* guaranteed to reach the model is a tool result it already asked
//! for. So every result carries a `notices` block, and the same lines appear in
//! the text half, which is the part clients reliably show. The model finds out
//! about the workspace as a side effect of doing its work.
//!
//! Do not "fix" this by moving it to server-initiated notifications. That
//! channel exists here too (`notifications/resources/list_changed`, sent from
//! the heartbeat thread) and is deliberately best-effort: nothing may depend on
//! a client acting on it.
//!
//! Polling the events table on a background thread is the mechanism, and it is
//! the same call the daemon's pump makes for the same reason — CLI invocations
//! and other agents in other processes write events too, so in-process
//! broadcasting would miss exactly the events that matter most.

use anyhow::Result;
use golab_core::model::*;
use golab_core::protocol::Direction;
use golab_core::Store;
use serde::Serialize;

/// Caps, because this rides along on every single tool call.
const MAX_INBOX: usize = 5;
const MAX_EVENTS: usize = 8;
/// A lease with less than this left is worth mentioning unprompted.
const EXPIRING_SOON_SECS: i64 = 60;

/// Event kinds that concern everybody, regardless of what they are holding.
const GLOBAL_INTEREST: &[&str] = &[
    "task.unblocked",
    "goal.created",
    "goal.state_changed",
    "session.started",
    "session.ended",
];

#[derive(Debug, Clone, Default, Serialize)]
pub struct Notices {
    /// Live requests addressed to you, or broadcast to everyone.
    pub inbox: Vec<Request>,
    /// What happened elsewhere that bears on your work.
    pub events: Vec<Event>,
    /// Your leases about to lapse. Renewal is automatic, so seeing these means
    /// something is wrong — usually that you have held something far longer
    /// than you meant to.
    pub expiring: Vec<Lease>,
    pub task: Option<String>,
    pub goal: Option<String>,
    /// Set when there is something to do about all this now.
    pub action_required: Option<String>,
}

impl Notices {
    pub fn is_empty(&self) -> bool {
        self.inbox.is_empty() && self.events.is_empty() && self.expiring.is_empty()
    }

    /// One line for the text half of a tool result. Empty when there is
    /// nothing to say, so a quiet workspace costs no tokens at all.
    pub fn summary(&self) -> String {
        if self.is_empty() {
            return String::new();
        }
        let mut parts: Vec<String> = Vec::new();
        for r in &self.inbox {
            parts.push(format!(
                "{} asks: {} ({}, reply with respond request={})",
                r.from, r.subject, r.kind, r.id
            ));
        }
        for e in &self.events {
            parts.push(describe(e));
        }
        for l in &self.expiring {
            parts.push(format!("your lease on {} is nearly up", l.symbol_handle));
        }
        format!("[golab] {}", parts.join(" · "))
    }
}

fn describe(e: &Event) -> String {
    let who = e.agent.as_deref().unwrap_or("someone");
    let what = e.symbol_handle.as_deref().unwrap_or("");
    match e.kind.as_str() {
        "lease.released" => format!("{who} released {what}"),
        "lease.expired" => format!("{what} lapsed and is free"),
        "lease.acquired" => format!("{who} took {what}"),
        "lease.transferred" => format!("{what} changed hands"),
        "task.unblocked" => format!(
            "{} is unblocked",
            e.task.as_deref().unwrap_or("a dependent task")
        ),
        "task.completed" => format!("{} finished", e.task.as_deref().unwrap_or("a task")),
        "request.fulfilled" => "a request of yours was fulfilled".to_string(),
        "request.declined" => "a request of yours was declined".to_string(),
        "session.started" => format!("{who} connected"),
        "session.ended" => format!("{who} disconnected"),
        other => format!("{other} {what}").trim().to_string(),
    }
}

/// Collect what `agent` should be told, and advance `cursor` past it.
pub fn collect(store: &Store, agent: &str, cursor: &mut i64) -> Result<Notices> {
    let mut notices = Notices::default();

    let inbox = store.requests(Some(agent), Direction::Inbox, true)?;
    notices.inbox = inbox.into_iter().take(MAX_INBOX).collect();

    let leases = store.active_leases(Some(agent))?;
    let now = golab_core::now_ms();
    notices.expiring = leases
        .iter()
        .filter(|l| l.seconds_left(now) <= EXPIRING_SOON_SECS)
        .cloned()
        .collect();

    let held: Vec<&str> = leases.iter().map(|l| l.symbol_handle.as_str()).collect();
    if let Some(view) = store.agents()?.into_iter().find(|a| a.agent.name == agent) {
        notices.task = view.current_task.clone();
        notices.goal = view.current_goal.clone();
    }

    let fresh = store.events_since(*cursor, 200)?;
    if let Some(last) = fresh.last() {
        *cursor = last.id;
    }
    notices.events = fresh
        .into_iter()
        .filter(|e| relevant(e, agent, &notices.task, &held))
        .take(MAX_EVENTS)
        .collect();

    notices.action_required = if !notices.inbox.is_empty() {
        Some(format!(
            "{} request(s) are waiting on you — call `inbox` and answer them.",
            notices.inbox.len()
        ))
    } else if !notices.expiring.is_empty() {
        Some("Some of your leases are nearly up; call `progress` or hand them back.".to_string())
    } else {
        None
    };

    Ok(notices)
}

fn relevant(e: &Event, agent: &str, task: &Option<String>, held: &[&str]) -> bool {
    // Never report an agent's own actions back to it. It knows; saying so
    // would fill the context window with an echo.
    if e.agent.as_deref() == Some(agent) {
        return false;
    }
    if GLOBAL_INTEREST.contains(&e.kind.as_str()) {
        return true;
    }
    if task.is_some() && e.task.as_deref() == task.as_deref() {
        return true;
    }
    if let Some(handle) = e.symbol_handle.as_deref() {
        if held.contains(&handle) {
            return true;
        }
    }
    // Anything that frees a symbol matters: an agent blocked on it can move.
    matches!(
        e.kind.as_str(),
        "lease.released" | "lease.expired" | "lease.transferred" | "task.completed"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use golab_core::lease::AcquireOptions;
    use golab_core::protocol::NewRequest;
    use serde_json::json;

    fn fixture() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("src/pay.ts"),
            "export function charge(x: number) { return x; }\n\
             export function refund(x: number) { return -x; }\n",
        )
        .unwrap();
        let mut store = Store::init(dir.path()).unwrap();
        golab_core::scan::scan(
            &mut store,
            golab_core::ids::DEFAULT_REPO_ID,
            dir.path(),
            &[],
            false,
        )
        .unwrap();
        store.register_agent("alice", "claude-code").unwrap();
        store.register_agent("bob", "cursor").unwrap();
        (dir, store)
    }

    #[test]
    fn a_quiet_workspace_costs_nothing() {
        let (_d, store) = fixture();
        let mut cursor = store.last_event_id().unwrap();
        let n = collect(&store, "alice", &mut cursor).unwrap();
        assert!(n.is_empty());
        assert_eq!(n.summary(), "", "an empty block must not be emitted at all");
    }

    #[test]
    fn a_request_for_you_rides_along_with_a_way_to_answer_it() {
        let (_d, mut store) = fixture();
        let mut cursor = store.last_event_id().unwrap();
        store
            .open_request(&NewRequest {
                to: Some("alice".to_string()),
                body: json!({}),
                ..NewRequest::new(request_kind::QUESTION, "bob", "can you look at refunds?")
            })
            .unwrap();

        let n = collect(&store, "alice", &mut cursor).unwrap();
        assert_eq!(n.inbox.len(), 1);
        let summary = n.summary();
        assert!(summary.contains("bob asks"), "{summary}");
        assert!(
            summary.contains("respond request="),
            "telling the model something without telling it how to reply is half a channel: {summary}"
        );
        assert!(n.action_required.is_some());
    }

    #[test]
    fn your_own_actions_are_not_reported_back_to_you() {
        let (_d, mut store) = fixture();
        let mut cursor = store.last_event_id().unwrap();
        store
            .acquire_ref("charge", "alice", &AcquireOptions::default())
            .unwrap();

        let n = collect(&store, "alice", &mut cursor).unwrap();
        assert!(
            n.events.is_empty(),
            "alice knows what alice just did: {:?}",
            n.events.iter().map(|e| &e.kind).collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_symbol_coming_free_reaches_whoever_was_waiting() {
        let (_d, mut store) = fixture();
        store
            .acquire_ref("charge", "bob", &AcquireOptions::default())
            .unwrap();
        let mut cursor = store.last_event_id().unwrap();

        let lease = store.active_leases(Some("bob")).unwrap()[0].id.clone();
        store.release(&lease, "bob").unwrap();

        let n = collect(&store, "alice", &mut cursor).unwrap();
        assert!(
            n.events.iter().any(|e| e.kind == "lease.released"),
            "an agent blocked on this can now move: {:?}",
            n.events.iter().map(|e| &e.kind).collect::<Vec<_>>()
        );
        assert!(n.summary().contains("released"));
    }

    #[test]
    fn the_cursor_advances_so_nothing_is_reported_twice() {
        let (_d, mut store) = fixture();
        store
            .acquire_ref("charge", "bob", &AcquireOptions::default())
            .unwrap();
        let mut cursor = store.last_event_id().unwrap();
        let lease = store.active_leases(Some("bob")).unwrap()[0].id.clone();
        store.release(&lease, "bob").unwrap();

        let first = collect(&store, "alice", &mut cursor).unwrap();
        assert!(!first.events.is_empty());
        let second = collect(&store, "alice", &mut cursor).unwrap();
        assert!(
            second.events.is_empty(),
            "repeating a notice on every call would drown the model"
        );
    }

    #[test]
    fn somebody_elses_unrelated_work_is_not_reported() {
        let (_d, mut store) = fixture();
        let mut cursor = store.last_event_id().unwrap();
        // Bob taking a symbol alice neither holds nor has a task on is simply
        // not her business — the whole value of this channel is that what
        // arrives is worth reading.
        store
            .acquire_ref("charge", "bob", &AcquireOptions::default())
            .unwrap();

        let n = collect(&store, "alice", &mut cursor).unwrap();
        assert!(
            n.events.is_empty(),
            "noise here costs context on every single tool call: {:?}",
            n.events.iter().map(|e| &e.kind).collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_lease_about_to_lapse_is_worth_mentioning() {
        let (_d, mut store) = fixture();
        store
            .acquire_ref(
                "charge",
                "alice",
                &AcquireOptions {
                    ttl_secs: 30,
                    ..Default::default()
                },
            )
            .unwrap();
        let mut cursor = store.last_event_id().unwrap();

        let n = collect(&store, "alice", &mut cursor).unwrap();
        assert_eq!(n.expiring.len(), 1);
        assert!(n.summary().contains("nearly up"));
    }

    #[test]
    fn notices_stay_capped() {
        let (_d, mut store) = fixture();
        let mut cursor = store.last_event_id().unwrap();
        for i in 0..20 {
            store
                .open_request(&NewRequest {
                    to: Some("alice".to_string()),
                    body: json!({}),
                    ..NewRequest::new(request_kind::QUESTION, "bob", &format!("q{i}"))
                })
                .unwrap();
        }
        let n = collect(&store, "alice", &mut cursor).unwrap();
        assert!(n.inbox.len() <= MAX_INBOX);
        assert!(n.events.len() <= MAX_EVENTS);
    }
}
