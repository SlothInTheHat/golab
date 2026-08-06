//! Live connections from coding tools.
//!
//! An `agents` row says who *exists*; a session says who is attached **right
//! now**, and through what. Keeping them apart is the point: a stale agent row
//! and a live MCP session are genuinely different states, and anyone watching a
//! swarm has to be able to tell "alice's editor is open" from "alice ran a CLI
//! command an hour ago". One agent can also hold more than one session at once
//! — an MCP server and an editor hook inside the same window — which is the
//! other reason this is a table rather than a few columns on `agents`.
//!
//! Sessions expire the way leases do, and for the same reason: nothing should
//! have to notice a crash. A tool killed with `kill -9` never closes its stdin,
//! so [`Store::expire_sessions`] — swept alongside `expire_due` — reaps it.

use anyhow::{anyhow, Result};
use rusqlite::{params, OptionalExtension, Row};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::ids;
use crate::store::{self, Store};
use crate::work::AGENT_ONLINE_MS;

/// A session that has not checked in for this long is presumed gone.
///
/// Deliberately the same window as agent liveness: a session is presence with
/// a transport attached, and if the two windows disagreed the runtime would
/// hold two contradictory opinions about who is here.
pub const SESSION_STALE_MS: i64 = AGENT_ONLINE_MS;

/// Transport labels. Free-form like `Agent::kind`, but these three are what
/// the runtime itself opens.
pub mod transport {
    /// A `atlas mcp` server: the long-lived adapter a coding tool speaks to.
    pub const MCP: &str = "mcp";
    /// A one-shot editor hook callback.
    pub const HOOK: &str = "hook";
    /// A human or a script driving the CLI directly.
    pub const CLI: &str = "cli";
}

/// Whether losing this transport means the agent is really gone.
///
/// Only an MCP server owns its agent's liveness: it is long-lived and runs the
/// heartbeat thread, so if it stops checking in the tool behind it has died.
/// Hook sessions are one-shot processes that exit within milliseconds by
/// design — their going quiet says nothing at all, and releasing leases on that
/// basis would rip work away from an agent that is merely idle.
fn owns_liveness(transport: &str) -> bool {
    transport == transport::MCP
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub agent: String,
    /// The coding tool on the other end: `claude-code`, `cursor`, `ci`, ...
    pub tool: String,
    /// One of [`transport`], though the column is free-form.
    pub transport: String,
    /// The host's own session identifier, when it tells us one. Editor hooks
    /// fire as separate short-lived processes, and this is the only thing
    /// tying them back to the session that started them.
    pub client_key: Option<String>,
    pub cwd: String,
    pub pid: Option<i64>,
    pub started_at: i64,
    pub heartbeat_at: i64,
    pub ended_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionView {
    #[serde(flatten)]
    pub session: Session,
    /// Not ended, and heartbeated inside [`SESSION_STALE_MS`].
    pub live: bool,
    pub uptime_secs: i64,
}

/// What a tool tells us when it attaches.
#[derive(Debug, Clone)]
pub struct NewSession {
    pub agent: String,
    pub tool: String,
    pub transport: String,
    pub client_key: Option<String>,
    pub cwd: String,
    pub pid: Option<i64>,
}

impl NewSession {
    pub fn new(agent: &str, tool: &str, transport: &str, cwd: &str) -> NewSession {
        NewSession {
            agent: agent.to_string(),
            tool: tool.to_string(),
            transport: transport.to_string(),
            client_key: None,
            cwd: cwd.to_string(),
            pid: None,
        }
    }
}

const SESSION_COLS: &str =
    "id, agent, tool, transport, client_key, cwd, pid, started_at, heartbeat_at, ended_at";

impl Store {
    /// Announce a live connection.
    ///
    /// Does not register the agent — the caller does that first, because
    /// `register_agent` is what decides the agent's `kind`.
    pub fn open_session(&mut self, s: &NewSession) -> Result<Session> {
        let s = s.clone();
        self.write(move |tx| {
            let now = ids::now_ms();
            let session = Session {
                id: ids::unique_id("sess"),
                agent: s.agent,
                tool: s.tool,
                transport: s.transport,
                client_key: s.client_key,
                cwd: s.cwd,
                pid: s.pid,
                started_at: now,
                heartbeat_at: now,
                ended_at: None,
            };
            tx.execute(
                "INSERT INTO sessions(id, agent, tool, transport, client_key, cwd, pid, \
                 started_at, heartbeat_at, ended_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8, NULL)",
                params![
                    session.id,
                    session.agent,
                    session.tool,
                    session.transport,
                    session.client_key,
                    session.cwd,
                    session.pid,
                    now,
                ],
            )?;
            store::emit(
                tx,
                "session.started",
                Some(&session.agent),
                None,
                None,
                json!({
                    "session": session.id,
                    "tool": session.tool,
                    "transport": session.transport,
                    "cwd": session.cwd,
                }),
            )?;
            Ok(session)
        })
    }

    /// Bump the session clock.
    ///
    /// Returns `false` when the session is already ended or was reaped, which
    /// is the signal for a long-running adapter to open a fresh one rather
    /// than heartbeat into a row nobody is watching.
    pub fn heartbeat_session(&mut self, id: &str) -> Result<bool> {
        let id = id.to_string();
        self.write(move |tx| {
            let n = tx.execute(
                "UPDATE sessions SET heartbeat_at = ?2 WHERE id = ?1 AND ended_at IS NULL",
                params![id, ids::now_ms()],
            )?;
            Ok(n > 0)
        })
    }

    /// Close a session cleanly.
    ///
    /// Idempotent: ending an already-ended session is not an error, because
    /// stdin EOF and an editor's own SessionEnd callback both legitimately
    /// arrive for the same session.
    ///
    /// The `agents` row survives. We end sessions; we do not delete people —
    /// a dashboard still wants to show who was here and when they left.
    pub fn end_session(&mut self, id: &str, release_leases: bool) -> Result<Session> {
        let existing = self
            .session(id)?
            .ok_or_else(|| anyhow!("no such session: {id}"))?;
        if existing.ended_at.is_some() {
            return Ok(existing);
        }

        // Only the agent's *last* attachment hands work back. An agent running
        // an MCP server and an editor hook holds two sessions, and whichever
        // closes first must not strip the other of the leases it is using.
        let last = self.live_sessions_for(&existing.agent, Some(id))? == 0;

        let sid = existing.id.clone();
        let agent = existing.agent.clone();
        let tool = existing.tool.clone();
        let via = existing.transport.clone();
        self.write(move |tx| {
            let now = ids::now_ms();
            tx.execute(
                "UPDATE sessions SET ended_at = ?2 WHERE id = ?1",
                params![sid, now],
            )?;
            store::emit(
                tx,
                "session.ended",
                Some(&agent),
                None,
                None,
                json!({ "session": sid, "tool": tool, "transport": via, "last": last }),
            )?;
            Ok(())
        })?;

        if last {
            // Whoever has left is no longer editing anything, whether or not
            // they kept their leases: `--keep-leases` means "I will be back for
            // this work", not "my hands are still on the keyboard".
            self.clear_activity(&existing.agent)?;
            if release_leases {
                self.release_all(&existing.agent)?;
            }
        }

        self.session(id)?
            .ok_or_else(|| anyhow!("session vanished: {id}"))
    }

    /// Reap sessions whose process died without closing stdin.
    ///
    /// The lazy-expiry twin of `expire_due`: nothing has to notice the crash,
    /// because whoever next writes, watches or ticks runs `sweep`.
    ///
    /// Leases are only handed back when the agent's last *liveness-owning*
    /// session goes stale — see [`owns_liveness`]. An expired hook session
    /// releases nothing.
    pub fn expire_sessions(&mut self) -> Result<usize> {
        let cutoff = ids::now_ms() - SESSION_STALE_MS;
        let stale: Vec<Session> = {
            let mut stmt = self.conn().prepare(&format!(
                "SELECT {SESSION_COLS} FROM sessions \
                 WHERE ended_at IS NULL AND heartbeat_at < ?1 ORDER BY started_at"
            ))?;
            let rows = stmt.query_map(params![cutoff], row_to_session)?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        if stale.is_empty() {
            return Ok(0);
        }

        let doomed = stale.clone();
        self.write(move |tx| {
            let now = ids::now_ms();
            for s in &doomed {
                tx.execute(
                    "UPDATE sessions SET ended_at = ?2 WHERE id = ?1",
                    params![s.id, now],
                )?;
                store::emit(
                    tx,
                    "session.expired",
                    Some(&s.agent),
                    None,
                    None,
                    json!({
                        "session": s.id,
                        "tool": s.tool,
                        "transport": s.transport,
                        "silent_for_secs": (now - s.heartbeat_at) / 1000,
                    }),
                )?;
            }
            Ok(())
        })?;

        // Now that the rows are closed, `live_sessions_for` gives the honest
        // answer to "is anything of this agent's still attached?".
        let mut abandoned: Vec<String> = stale
            .iter()
            .filter(|s| owns_liveness(&s.transport))
            .map(|s| s.agent.clone())
            .collect();
        abandoned.sort();
        abandoned.dedup();
        for agent in abandoned {
            if self.live_sessions_for(&agent, None)? == 0 {
                self.release_all(&agent)?;
            }
        }

        Ok(stale.len())
    }

    pub fn session(&self, id: &str) -> Result<Option<Session>> {
        Ok(self
            .conn()
            .query_row(
                &format!("SELECT {SESSION_COLS} FROM sessions WHERE id = ?1"),
                params![id],
                row_to_session,
            )
            .optional()?)
    }

    /// Find a session by the host's own identifier. How an editor hook, which
    /// runs in its own process and knows nothing of our ids, finds the session
    /// its SessionStart opened.
    pub fn session_by_client_key(&self, key: &str) -> Result<Option<Session>> {
        Ok(self
            .conn()
            .query_row(
                &format!(
                    "SELECT {SESSION_COLS} FROM sessions WHERE client_key = ?1 \
                     ORDER BY ended_at IS NULL DESC, started_at DESC LIMIT 1"
                ),
                params![key],
                row_to_session,
            )
            .optional()?)
    }

    /// Newest first, so the current attachment leads.
    pub fn sessions(&self, live_only: bool) -> Result<Vec<SessionView>> {
        let mut stmt = self.conn().prepare(&format!(
            "SELECT {SESSION_COLS} FROM sessions ORDER BY started_at DESC"
        ))?;
        let rows = stmt.query_map([], row_to_session)?;
        Ok(view_all(rows.collect::<Result<Vec<_>, _>>()?, live_only))
    }

    pub fn sessions_for(&self, agent: &str) -> Result<Vec<SessionView>> {
        let mut stmt = self.conn().prepare(&format!(
            "SELECT {SESSION_COLS} FROM sessions WHERE agent = ?1 ORDER BY started_at DESC"
        ))?;
        let rows = stmt.query_map(params![agent], row_to_session)?;
        Ok(view_all(rows.collect::<Result<Vec<_>, _>>()?, false))
    }

    /// How many of `agent`'s sessions are still attached, optionally ignoring
    /// one that is about to close.
    pub fn live_sessions_for(&self, agent: &str, excluding: Option<&str>) -> Result<usize> {
        let cutoff = ids::now_ms() - SESSION_STALE_MS;
        let n: i64 = self.conn().query_row(
            "SELECT COUNT(*) FROM sessions \
             WHERE agent = ?1 AND ended_at IS NULL AND heartbeat_at >= ?2 AND id IS NOT ?3",
            params![agent, cutoff, excluding],
            |r| r.get(0),
        )?;
        Ok(n as usize)
    }
}

fn view_all(sessions: Vec<Session>, live_only: bool) -> Vec<SessionView> {
    let now = ids::now_ms();
    sessions
        .into_iter()
        .map(|s| SessionView {
            live: s.ended_at.is_none() && now - s.heartbeat_at <= SESSION_STALE_MS,
            uptime_secs: (s.ended_at.unwrap_or(now) - s.started_at) / 1000,
            session: s,
        })
        .filter(|v| !live_only || v.live)
        .collect()
}

fn row_to_session(r: &Row) -> rusqlite::Result<Session> {
    Ok(Session {
        id: r.get(0)?,
        agent: r.get(1)?,
        tool: r.get(2)?,
        transport: r.get(3)?,
        client_key: r.get(4)?,
        cwd: r.get(5)?,
        pid: r.get(6)?,
        started_at: r.get(7)?,
        heartbeat_at: r.get(8)?,
        ended_at: r.get(9)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("src/pay.ts"),
            "export function charge(x: number) { return x; }\n\
             export function refund(x: number) { return x; }\n",
        )
        .unwrap();
        let mut store = Store::init(dir.path()).unwrap();
        crate::scan::scan(&mut store, crate::ids::DEFAULT_REPO_ID, dir.path(), &[], false).unwrap();
        store.register_agent("alice", "claude-code").unwrap();
        (dir, store)
    }

    fn attach(store: &mut Store, agent: &str, transport: &str) -> Session {
        store
            .open_session(&NewSession::new(agent, "claude-code", transport, "/repo"))
            .unwrap()
    }

    /// Backdate a session's clock past the stale window without sleeping.
    fn silence(store: &mut Store, id: &str) {
        let stale = ids::now_ms() - SESSION_STALE_MS - 1_000;
        store
            .conn()
            .execute(
                "UPDATE sessions SET heartbeat_at = ?2 WHERE id = ?1",
                params![id, stale],
            )
            .unwrap();
    }

    #[test]
    fn a_session_is_live_until_it_is_ended() {
        let (_d, mut store) = fixture();
        let s = attach(&mut store, "alice", transport::MCP);

        let live = store.sessions(true).unwrap();
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].session.id, s.id);
        assert!(live[0].live);

        store.end_session(&s.id, true).unwrap();
        assert!(store.sessions(true).unwrap().is_empty());
        assert_eq!(store.sessions(false).unwrap().len(), 1, "history is kept");
    }

    #[test]
    fn ending_a_session_leaves_the_agent_row_behind() {
        let (_d, mut store) = fixture();
        let s = attach(&mut store, "alice", transport::MCP);
        store.end_session(&s.id, true).unwrap();

        assert!(
            store.agent("alice").unwrap().is_some(),
            "we end sessions, not people — the dashboard still shows who was here"
        );
    }

    #[test]
    fn ending_twice_is_not_an_error() {
        let (_d, mut store) = fixture();
        let s = attach(&mut store, "alice", transport::MCP);
        let first = store.end_session(&s.id, true).unwrap();
        let second = store.end_session(&s.id, true).unwrap();
        assert_eq!(first.ended_at, second.ended_at);
    }

    #[test]
    fn the_last_session_to_close_releases_the_leases() {
        let (_d, mut store) = fixture();
        let mcp = attach(&mut store, "alice", transport::MCP);
        let hook = attach(&mut store, "alice", transport::HOOK);
        store.acquire_ref("charge", "alice", &Default::default()).unwrap();
        assert_eq!(store.active_leases(None).unwrap().len(), 1);

        // The editor hook exiting must not strip the MCP server of its work.
        store.end_session(&hook.id, true).unwrap();
        assert_eq!(
            store.active_leases(None).unwrap().len(),
            1,
            "another session of this agent's is still attached"
        );

        store.end_session(&mcp.id, true).unwrap();
        assert!(store.active_leases(None).unwrap().is_empty());
    }

    #[test]
    fn keeping_leases_survives_a_clean_close() {
        let (_d, mut store) = fixture();
        let s = attach(&mut store, "alice", transport::MCP);
        store.acquire_ref("charge", "alice", &Default::default()).unwrap();

        store.end_session(&s.id, false).unwrap();
        assert_eq!(
            store.active_leases(None).unwrap().len(),
            1,
            "a session that will resume keeps what it holds"
        );
    }

    #[test]
    fn a_silent_mcp_session_is_reaped_and_hands_work_back() {
        let (_d, mut store) = fixture();
        let s = attach(&mut store, "alice", transport::MCP);
        store.acquire_ref("charge", "alice", &Default::default()).unwrap();
        silence(&mut store, &s.id);

        assert_eq!(store.expire_sessions().unwrap(), 1);
        assert!(store.session(&s.id).unwrap().unwrap().ended_at.is_some());
        assert!(
            store.active_leases(None).unwrap().is_empty(),
            "a kill -9'd adapter never closes stdin; the sweep is what frees its work"
        );
    }

    #[test]
    fn a_silent_hook_session_releases_nothing() {
        let (_d, mut store) = fixture();
        let s = attach(&mut store, "alice", transport::HOOK);
        store.acquire_ref("charge", "alice", &Default::default()).unwrap();
        silence(&mut store, &s.id);

        assert_eq!(store.expire_sessions().unwrap(), 1);
        assert_eq!(
            store.active_leases(None).unwrap().len(),
            1,
            "hook processes exit within milliseconds by design — their silence means nothing"
        );
    }

    #[test]
    fn heartbeating_a_reaped_session_reports_it_is_gone() {
        let (_d, mut store) = fixture();
        let s = attach(&mut store, "alice", transport::MCP);
        assert!(store.heartbeat_session(&s.id).unwrap());

        silence(&mut store, &s.id);
        store.expire_sessions().unwrap();
        assert!(
            !store.heartbeat_session(&s.id).unwrap(),
            "the adapter needs to know to open a fresh session, not talk to a closed one"
        );
    }

    #[test]
    fn a_heartbeat_keeps_a_session_out_of_the_sweep() {
        let (_d, mut store) = fixture();
        let s = attach(&mut store, "alice", transport::MCP);
        silence(&mut store, &s.id);
        assert!(store.heartbeat_session(&s.id).unwrap());

        assert_eq!(store.expire_sessions().unwrap(), 0);
        assert!(store.session(&s.id).unwrap().unwrap().ended_at.is_none());
    }

    #[test]
    fn a_hook_finds_its_session_by_the_hosts_own_id() {
        let (_d, mut store) = fixture();
        let opened = store
            .open_session(&NewSession {
                client_key: Some("abc123".to_string()),
                ..NewSession::new("alice", "claude-code", transport::HOOK, "/repo")
            })
            .unwrap();

        let found = store.session_by_client_key("abc123").unwrap().unwrap();
        assert_eq!(found.id, opened.id);
        assert!(store.session_by_client_key("nope").unwrap().is_none());
    }

    #[test]
    fn two_tools_are_two_sessions_under_two_identities() {
        let (_d, mut store) = fixture();
        store.register_agent("bob", "cursor").unwrap();
        attach(&mut store, "alice", transport::MCP);
        store
            .open_session(&NewSession::new("bob", "cursor", transport::MCP, "/repo"))
            .unwrap();

        assert_eq!(store.sessions(true).unwrap().len(), 2);
        assert_eq!(store.sessions_for("alice").unwrap().len(), 1);
        assert_eq!(store.live_sessions_for("bob", None).unwrap(), 1);
    }

    #[test]
    fn sweep_reaps_sessions_alongside_leases_and_requests() {
        let (_d, mut store) = fixture();
        let s = attach(&mut store, "alice", transport::MCP);
        silence(&mut store, &s.id);

        let report = store.sweep().unwrap();
        assert_eq!(report.sessions, 1, "expiry belongs in sweep, not in each command");
    }
}
