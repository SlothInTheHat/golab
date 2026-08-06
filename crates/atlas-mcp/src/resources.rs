//! Read-only views of the workspace, addressable by URI.
//!
//! Resources are the pull half of the story: state a client can fetch or
//! re-fetch without the model spending a tool call, and the thing a client
//! re-reads when it sees `notifications/resources/list_changed`. Everything
//! here is also reachable through a tool, deliberately — clients differ wildly
//! in whether they expose resources to the model at all, so nothing may depend
//! on them.

use anyhow::{anyhow, Result};
use atlas_core::protocol::Direction;
use serde_json::{json, Value};

use crate::server::Session;

const RESOURCES: &[(&str, &str, &str)] = &[
    (
        "atlas://status",
        "workspace status",
        "Everything at a glance: agents, leases, tasks, open negotiations, scheduler headline.",
    ),
    (
        "atlas://goals",
        "goals and progress",
        "What the humans actually asked for, and how far along each one is.",
    ),
    (
        "atlas://plan",
        "schedule",
        "What can run now, what runs after, what is blocked, and the critical path.",
    ),
    (
        "atlas://inbox",
        "your inbox",
        "Structured requests addressed to you that are still live.",
    ),
    (
        "atlas://context",
        "your context",
        "Your current task and its surroundings, or what you could pick up if idle.",
    ),
];

pub fn list() -> Value {
    let resources: Vec<Value> = RESOURCES
        .iter()
        .map(|(uri, name, description)| {
            json!({
                "uri": uri,
                "name": name,
                "description": description,
                "mimeType": "application/json",
            })
        })
        .collect();
    json!({ "resources": resources })
}

pub fn read(s: &mut Session, params: &Value) -> Result<Value> {
    let uri = params
        .get("uri")
        .and_then(|u| u.as_str())
        .ok_or_else(|| anyhow!("resources/read needs a uri"))?;

    let body = match uri {
        "atlas://status" => serde_json::to_value(s.store.status(30)?)?,
        "atlas://goals" => {
            let mut out = Vec::new();
            for goal in s.store.goals()? {
                let progress = s.store.goal_progress(&goal.id)?;
                out.push(json!({ "goal": goal, "progress": progress }));
            }
            json!(out)
        }
        "atlas://plan" => serde_json::to_value(s.store.plan()?)?,
        "atlas://inbox" => serde_json::to_value(s.store.requests(
            Some(&s.agent),
            Direction::Inbox,
            true,
        )?)?,
        "atlas://context" => serde_json::to_value(s.store.agent_context(&s.agent, 2)?)?,
        other => return Err(anyhow!("unknown resource: {other}")),
    };

    Ok(json!({
        "contents": [{
            "uri": uri,
            "mimeType": "application/json",
            "text": serde_json::to_string_pretty(&body)?,
        }]
    }))
}
