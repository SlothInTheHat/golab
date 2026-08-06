//! Editor hook callbacks.
//!
//! These are what make the integration deterministic rather than advisory.
//! An MCP tool is only ever called if the model decides to call it; a hook
//! fires whether the model cooperates or not, which is the difference between
//! "please check before editing" and "you cannot edit this".
//!
//! Each callback reads the host's event payload on stdin and answers on
//! stdout. They are hidden subcommands — a human never runs one.
//!
//! ## The external contract, and why it is hedged
//!
//! The structured decision format below is Claude Code's, and it is the one
//! thing here we do not control. So a denial is expressed **twice**:
//!
//! 1. as `hookSpecificOutput.permissionDecision = "deny"` on stdout, and
//! 2. as exit code 2 with the reason on stderr — the older, simpler contract,
//!    which is documented to feed stderr back to the model.
//!
//! A client that understands the JSON uses it; one that does not still blocks
//! on the exit code, and the model still sees why. `--mode` picks between
//! them, so if the format changes it is a flag flip rather than a rewrite.
//! Every field name lives in this module and nowhere else.
//!
//! ## Failing open
//!
//! Every callback except `guard` exits 0 no matter what goes wrong. A
//! coordination layer that bricks somebody's editor because SQLite was briefly
//! busy has failed at the job it exists to do.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::Result;
use golab_core::activity::{self, NewActivity};
use golab_core::guard::GuardVerdict;
use golab_core::identity;
use golab_core::session::{transport, NewSession};
use golab_core::workspace::{guard_workspace, locate};
use golab_core::Store;
use serde_json::{json, Value};

/// The tool label an editor hook registers under. Hooks and the MCP server in
/// the same window must derive the *same* agent name, and they have no way to
/// tell each other what they picked — so both start from this.
const CLAUDE_CODE: &str = "claude-code";

pub fn read_payload() -> Value {
    let mut raw = String::new();
    // No payload at all is not an error: it just means there is nothing to
    // decide about, and every callback handles the empty case.
    if std::io::stdin().read_to_string(&mut raw).is_err() {
        return json!({});
    }
    serde_json::from_str(&raw).unwrap_or_else(|_| json!({}))
}

fn str_at<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(|x| x.as_str()).filter(|s| !s.is_empty())
}

/// Who this editor session is.
///
/// Derived from the same inputs `golab mcp` uses, so a hook process and the
/// MCP server running in the same window independently arrive at the same
/// name — they have no channel to agree over. The payload's `tool_name` is the
/// *tool being called* (`Edit`, `Write`), not the client, so it is no help
/// here; the client is always Claude Code by construction, since these hooks
/// only exist in `.claude/settings.json`.
fn agent_for(_payload: &Value) -> String {
    identity::derive_agent(CLAUDE_CODE, None)
}

// ------------------------------------------------------------------ PreToolUse

/// Every path a single tool call is about to touch.
///
/// Covering all four editing tools matters: an agent that can reach around the
/// guard by using `MultiEdit` instead of `Edit` is not guarded at all.
fn edited_paths(payload: &Value) -> Vec<PathBuf> {
    let input = payload.get("tool_input").cloned().unwrap_or(json!({}));
    let mut out: Vec<PathBuf> = Vec::new();
    let mut push = |p: Option<&str>| {
        if let Some(p) = p {
            let path = PathBuf::from(p);
            if !out.contains(&path) {
                out.push(path);
            }
        }
    };
    push(str_at(&input, "file_path"));
    push(str_at(&input, "notebook_path"));
    if let Some(edits) = input.get("edits").and_then(|e| e.as_array()) {
        for e in edits {
            push(str_at(e, "file_path"));
        }
    }
    out
}

pub fn guard(root: &Path, mode: &str) -> Result<ExitCode> {
    let payload = read_payload();
    let paths = edited_paths(&payload);
    if paths.is_empty() {
        return Ok(allow(None));
    }

    let agent = agent_for(&payload);
    let Ok(mut store) = Store::open(&golab_core::db_path(root)) else {
        // No workspace, or a database we cannot read. Not this hook's problem.
        return Ok(allow(None));
    };

    // Worst verdict across every path the call touches: one denied file is
    // enough to refuse the whole call, because the call is atomic.
    let mut worst: Option<golab_core::guard::GuardReport> = None;
    for path in &paths {
        match guard_workspace(&store, root, &agent, path, None) {
            Ok(report) => {
                let replace = match &worst {
                    None => true,
                    Some(w) => rank(report.verdict) > rank(w.verdict),
                };
                if replace {
                    worst = Some(report);
                }
            }
            // A path we cannot judge is a path we do not block on.
            Err(_) => continue,
        }
    }

    let Some(report) = worst else {
        return Ok(allow(None));
    };

    // This is the moment nothing else in the runtime can see: an agent is about
    // to change a specific file, and has not yet. Recording it here is what
    // lets a second person watching the dashboard know to stay out of that file
    // *before* the edit lands, rather than reading about it afterwards.
    record_edit_window(&mut store, &agent, &report);

    if !report.blocking() {
        // Deliberately silent on `warn`: emitting an explicit "allow" would
        // short-circuit the user's own permission rules, and an unleased edit
        // is legal. The pre-commit hook is still there to catch it later.
        return Ok(allow(None));
    }

    let reason = deny_reason(&report);
    emit_deny(mode, &reason);
    Ok(match mode {
        "json" => ExitCode::SUCCESS,
        // Exit 2 with the reason on stderr: the oldest and most portable way
        // to both block the call and get the explanation in front of the model.
        _ => ExitCode::from(2),
    })
}

/// Open an edit window from a guard report.
///
/// Best-effort by construction: a failed write here must never change the
/// verdict, the exit code, or how long the editor waits. The report already
/// carries the repo routing and the resolved symbol, so this costs one insert
/// and no extra lookups.
fn record_edit_window(store: &mut Store, agent: &str, report: &golab_core::guard::GuardReport) {
    let kind = if report.blocking() {
        // A refused edit is exactly the contention worth showing a human: two
        // people wanted the same file within one TTL.
        activity::kind::BLOCKED
    } else {
        activity::kind::EDITING
    };
    store
        .record_activity(&NewActivity {
            symbol_id: report.anchor.clone(),
            symbol_handle: report.anchor_handle.clone(),
            verdict: Some(verdict_label(report.verdict).to_string()),
            ..NewActivity::new(agent, &report.repo_id, &report.path, kind)
        })
        .ok();
}

fn verdict_label(v: GuardVerdict) -> &'static str {
    match v {
        GuardVerdict::Allowed => "allowed",
        GuardVerdict::Warn => "warn",
        GuardVerdict::Denied => "denied",
    }
}

fn rank(v: GuardVerdict) -> u8 {
    match v {
        GuardVerdict::Allowed => 0,
        GuardVerdict::Warn => 1,
        GuardVerdict::Denied => 2,
    }
}

/// The whole point of the denial: not "no", but "no, and here is how to get to
/// yes without a human brokering it".
fn deny_reason(report: &golab_core::guard::GuardReport) -> String {
    let mut msg = format!("golab: {}.", report.summary);
    if let Some(s) = report
        .suggestions
        .iter()
        .find(|s| s.action == "request-transfer" || s.action == "wait")
    {
        match s.action.as_str() {
            "wait" => msg.push_str(&format!(
                " It frees up in about {}s — wait rather than interrupt them.",
                s.seconds_until_free.unwrap_or(0).max(0)
            )),
            _ => msg.push_str(&format!(
                " Ask for it with the golab `ask` tool (kind=lease-transfer, symbol=\"{}\"), \
                 or run: {}",
                s.symbol, s.command
            )),
        }
    }
    if let Some(s) = report.suggestions.iter().find(|s| s.action == "narrow") {
        msg.push_str(&format!(
            " If your change does not touch {}, you may edit a different symbol in this file \
             instead — re-check with the `check_edit` tool naming that symbol.",
            s.symbol
        ));
    }
    msg
}

fn emit_deny(mode: &str, reason: &str) {
    if mode != "exit" {
        println!(
            "{}",
            json!({
                "hookSpecificOutput": {
                    "hookEventName": "PreToolUse",
                    "permissionDecision": "deny",
                    "permissionDecisionReason": reason,
                }
            })
        );
    }
    if mode != "json" {
        eprintln!("{reason}");
    }
}

fn allow(context: Option<String>) -> ExitCode {
    if let Some(text) = context {
        println!(
            "{}",
            json!({
                "hookSpecificOutput": {
                    "hookEventName": "SessionStart",
                    "additionalContext": text,
                }
            })
        );
    }
    ExitCode::SUCCESS
}

// ---------------------------------------------------------------- SessionStart

pub fn session_start(root: &Path) -> Result<ExitCode> {
    let payload = read_payload();
    let agent = agent_for(&payload);
    let Ok(mut store) = Store::open(&golab_core::db_path(root)) else {
        return Ok(allow(None));
    };

    if store.register_agent(&agent, CLAUDE_CODE).is_err() {
        return Ok(allow(None));
    }
    let cwd = str_at(&payload, "cwd")
        .map(|c| c.to_string())
        .unwrap_or_else(|| root.to_string_lossy().to_string());
    store
        .open_session(&NewSession {
            // The host's own id, so `session-end` can find this row from a
            // completely separate process later.
            client_key: str_at(&payload, "session_id").map(|s| s.to_string()),
            pid: Some(std::process::id() as i64),
            ..NewSession::new(&agent, CLAUDE_CODE, transport::HOOK, &cwd)
        })
        .ok();

    // Open the session already oriented, rather than waiting for the model to
    // think to ask.
    let context = store.agent_context(&agent, 2).ok().map(|c| brief(&agent, &c));
    Ok(allow(context))
}

fn brief(agent: &str, c: &golab_core::context::AgentContext) -> String {
    let mut out = format!(
        "This repository is coordinated by golab; you are `{agent}`. \
         Other people and agents may be editing it right now. Call the golab \
         `check_edit` tool before editing a file, and do not edit anything it \
         reports as denied.\n"
    );
    if let Some(task) = &c.task {
        out.push_str(&format!(
            "You are on {}: {}. Scope: {}.\n",
            task.task.task.id,
            task.task.task.title,
            task.scope
                .iter()
                .map(|s| s.symbol.handle())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    } else if !c.startable.is_empty() {
        out.push_str(&format!(
            "{} task(s) are ready to start; call the golab `next_task` tool to take one.\n",
            c.startable.len()
        ));
    }
    if !c.notices.is_empty() {
        out.push_str(&format!(
            "{} unread request(s) from other agents — call `inbox`.\n",
            c.notices.len()
        ));
    }
    if !c.memory.is_empty() {
        out.push_str("Team decisions already recorded:\n");
        for m in c.memory.iter().take(5) {
            out.push_str(&format!("- {}: {}\n", m.key, m.value));
        }
    }
    out
}

// ----------------------------------------------------------------- PostToolUse

/// Publish progress and renew leases after an edit lands.
///
/// This is where liveness stops depending on the model: `record_progress`
/// heartbeats and renews every held lease as a side effect, so an agent that
/// is visibly editing files cannot be reaped as dead.
pub fn post_tool(root: &Path) -> Result<ExitCode> {
    let payload = read_payload();
    let paths = edited_paths(&payload);
    let agent = agent_for(&payload);

    let Ok(mut store) = Store::open(&golab_core::db_path(root)) else {
        return Ok(ExitCode::SUCCESS);
    };
    let task = store
        .agents()
        .ok()
        .and_then(|a| a.into_iter().find(|a| a.agent.name == agent))
        .and_then(|a| a.current_task);

    // Close the loop on every path the call touched, not just the first: the
    // window each one opened in `guard` was per-path, so leaving the others
    // reading "editing" until they expire would be a lie for a whole minute.
    let mut anchor: Option<(String, String)> = None;
    for path in &paths {
        let Ok((repo_id, rel)) = locate(&store, root, path) else {
            continue;
        };
        let symbol_id = golab_core::ids::file_symbol_id(&repo_id, &rel);
        // Only claim a symbol that is actually indexed — a brand new file has
        // an id we can compute but nothing to point at.
        let indexed = matches!(store.symbol(&symbol_id), Ok(Some(_)));
        store
            .record_activity(&NewActivity {
                symbol_id: indexed.then(|| symbol_id.clone()),
                symbol_handle: indexed.then(|| rel.clone()),
                task: task.clone(),
                ..NewActivity::new(&agent, &repo_id, &rel, activity::kind::EDITED)
            })
            .ok();
        if anchor.is_none() && indexed {
            anchor = Some((symbol_id, rel));
        }
    }

    // Naming the symbol is what turns "alice: 55%" into "alice is editing
    // src/pay.ts". The column has always been there; nothing was filling it.
    let (symbol_id, note) = match &anchor {
        Some((id, rel)) => (Some(id.as_str()), format!("edited {rel}")),
        None => match paths.first() {
            Some(p) => (None, format!("edited {}", p.display())),
            None => (None, "edited".to_string()),
        },
    };
    store
        .record_progress(&agent, task.as_deref(), symbol_id, None, None, Some(&note))
        .ok();
    Ok(ExitCode::SUCCESS)
}

// ------------------------------------------------------------------ SessionEnd

pub fn session_end(root: &Path) -> Result<ExitCode> {
    let payload = read_payload();
    let Ok(mut store) = Store::open(&golab_core::db_path(root)) else {
        return Ok(ExitCode::SUCCESS);
    };

    // Find the row SessionStart opened, by the host's own session id.
    if let Some(key) = str_at(&payload, "session_id") {
        if let Ok(Some(session)) = store.session_by_client_key(key) {
            store.end_session(&session.id, true).ok();
            return Ok(ExitCode::SUCCESS);
        }
    }
    // No client key to match on: close this agent's hook sessions rather than
    // leave them to be reaped a minute later.
    let agent = agent_for(&payload);
    if let Ok(sessions) = store.sessions_for(&agent) {
        for s in sessions
            .iter()
            .filter(|s| s.live && s.session.transport == transport::HOOK)
        {
            store.end_session(&s.session.id, true).ok();
        }
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_editing_tool_shape_is_understood() {
        // Missing any of these is a hole an agent walks straight through.
        let edit = json!({ "tool_input": { "file_path": "/repo/src/pay.ts" } });
        assert_eq!(edited_paths(&edit), vec![PathBuf::from("/repo/src/pay.ts")]);

        let notebook = json!({ "tool_input": { "notebook_path": "/repo/a.ipynb" } });
        assert_eq!(edited_paths(&notebook), vec![PathBuf::from("/repo/a.ipynb")]);

        let multi = json!({ "tool_input": { "edits": [
            { "file_path": "/repo/a.ts" },
            { "file_path": "/repo/b.ts" },
            { "file_path": "/repo/a.ts" }
        ] } });
        assert_eq!(
            edited_paths(&multi),
            vec![PathBuf::from("/repo/a.ts"), PathBuf::from("/repo/b.ts")],
            "duplicates collapse; a file is judged once"
        );
    }

    #[test]
    fn a_payload_we_do_not_recognise_yields_nothing_to_guard() {
        assert!(edited_paths(&json!({})).is_empty());
        assert!(edited_paths(&json!({ "tool_input": { "command": "ls" } })).is_empty());
    }

    #[test]
    fn a_denial_says_how_to_get_to_yes() {
        use golab_core::guard::{GuardReport, GuardSuggestion};
        let report = GuardReport {
            agent: "bob".into(),
            repo_id: "R1".into(),
            path: "src/pay.ts".into(),
            anchor: Some("s_x".into()),
            anchor_handle: Some("src/pay.ts:charge".into()),
            verdict: GuardVerdict::Denied,
            conflicts: Vec::new(),
            lease_id: None,
            via: None,
            yours_within: Vec::new(),
            unindexed: false,
            suggestions: vec![GuardSuggestion {
                action: "request-transfer".into(),
                symbol: "src/pay.ts:charge".into(),
                holder: Some("alice".into()),
                seconds_until_free: Some(200),
                command: "golab request lease src/pay.ts:charge".into(),
                tool: "ask".into(),
            }],
            summary: "src/pay.ts:charge is held by alice for another 3m 20s".into(),
        };

        let reason = deny_reason(&report);
        assert!(reason.contains("alice"), "{reason}");
        assert!(
            reason.contains("lease-transfer"),
            "the model has to be told how to negotiate, not just told no: {reason}"
        );
    }

    #[test]
    fn an_about_to_lapse_lease_says_wait_rather_than_ask() {
        use golab_core::guard::{GuardReport, GuardSuggestion};
        let report = GuardReport {
            agent: "bob".into(),
            repo_id: "R1".into(),
            path: "src/pay.ts".into(),
            anchor: None,
            anchor_handle: None,
            verdict: GuardVerdict::Denied,
            conflicts: Vec::new(),
            lease_id: None,
            via: None,
            yours_within: Vec::new(),
            unindexed: false,
            suggestions: vec![GuardSuggestion {
                action: "wait".into(),
                symbol: "src/pay.ts:charge".into(),
                holder: Some("alice".into()),
                seconds_until_free: Some(4),
                command: "golab lease acquire src/pay.ts:charge --wait 9".into(),
                tool: "claim_symbol".into(),
            }],
            summary: "held by alice".into(),
        };
        assert!(deny_reason(&report).contains("wait rather than interrupt"));
    }

    #[test]
    fn the_worst_verdict_across_a_multi_file_call_wins() {
        assert!(rank(GuardVerdict::Denied) > rank(GuardVerdict::Warn));
        assert!(rank(GuardVerdict::Warn) > rank(GuardVerdict::Allowed));
    }
}
