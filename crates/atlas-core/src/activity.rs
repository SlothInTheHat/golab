//! What everyone is editing *right now*.
//!
//! Three tables describe an agent's relationship to code, and they answer
//! genuinely different questions:
//!
//! - a **lease** says what an agent *owns* — it outlives the edit that
//!   motivated it, and is the thing enforcement checks;
//! - a **progress** row says how far along the agent claims to be — a point in
//!   time, appended forever, the input to throughput;
//! - **activity** says where the agent's hands are this second.
//!
//! Only the third can answer "is anyone in `src/pay.ts` right now?", which is
//! the question a second human needs answered *before* they start typing. A
//! lease is too slow a signal (an agent holds one for the whole task) and
//! progress is too coarse (it names a percentage, not a file).
//!
//! # Where the signal comes from
//!
//! atlas does not watch keystrokes and never reads an editor's unsaved buffer.
//! It learns about an edit when a tool tells it, which happens at two moments
//! that are already load-bearing for other reasons:
//!
//! - the **PreToolUse hook**, which asks permission one keystroke before the
//!   edit lands — recorded as [`kind::EDITING`], or [`kind::BLOCKED`] when the
//!   guard refuses;
//! - the **PostToolUse hook** and the MCP `progress` tool, once it has landed —
//!   [`kind::EDITED`].
//!
//! So the fidelity is file-granular and sub-second for any tool wired up with
//! MCP or hooks, and absent for one wired up with neither. That is a real
//! limit and it is documented in the README rather than papered over.
//!
//! # Why rows expire instead of being deleted
//!
//! Same reason sessions do: nothing should have to notice a crash. A tool that
//! dies mid-edit never says "I stopped", so [`Store::expire_activity`] runs
//! inside [`Store::sweep`] and the window closes on its own.

use anyhow::Result;
use rusqlite::{params, Row};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::ids;
use crate::store::{self, Store};
use crate::work::AGENT_ONLINE_MS;

/// How long an edit-in-flight keeps counting as live.
///
/// Deliberately the same window as agent liveness and session staleness: a
/// tool that has gone quiet should stop claiming to be mid-edit at exactly the
/// moment it stops claiming to be present at all. Three different windows
/// would let the dashboard show an agent as offline while still "editing".
pub const ACTIVITY_LIVE_MS: i64 = AGENT_ONLINE_MS;

/// What the agent is doing to the file. Free-form in the column; these four
/// are what the runtime itself writes.
pub mod kind {
    /// Looked at it — a guard check that named no edit, or an explicit open.
    pub const OPENED: &str = "opened";
    /// About to change it. Written *before* the edit, from the pre-edit hook.
    pub const EDITING: &str = "editing";
    /// Changed it. Written after the edit landed.
    pub const EDITED: &str = "edited";
    /// Wanted to change it and was refused — somebody else holds it.
    pub const BLOCKED: &str = "blocked";
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Activity {
    pub id: i64,
    pub agent: String,
    pub session_id: Option<String>,
    pub repo_id: String,
    pub path: String,
    pub symbol_id: Option<String>,
    /// Human-readable form of `symbol_id`, denormalized so a reader does not
    /// need a join to render a row — the same trick `events` uses.
    pub symbol_handle: Option<String>,
    /// One of [`kind`].
    pub kind: String,
    pub task: Option<String>,
    /// The guard's answer at that moment, when there was one.
    pub verdict: Option<String>,
    pub note: Option<String>,
    /// When this agent first touched this path in the current window.
    pub started_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActivityView {
    #[serde(flatten)]
    pub activity: Activity,
    /// Updated inside [`ACTIVITY_LIVE_MS`].
    pub live: bool,
    pub age_secs: i64,
}

/// What a tool reports when it touches a file.
#[derive(Debug, Clone)]
pub struct NewActivity {
    pub agent: String,
    pub repo_id: String,
    pub path: String,
    pub kind: String,
    pub session_id: Option<String>,
    pub symbol_id: Option<String>,
    pub symbol_handle: Option<String>,
    pub task: Option<String>,
    pub verdict: Option<String>,
    pub note: Option<String>,
}

impl NewActivity {
    pub fn new(agent: &str, repo_id: &str, path: &str, kind: &str) -> NewActivity {
        NewActivity {
            agent: agent.to_string(),
            repo_id: repo_id.to_string(),
            path: path.to_string(),
            kind: kind.to_string(),
            session_id: None,
            symbol_id: None,
            symbol_handle: None,
            task: None,
            verdict: None,
            note: None,
        }
    }
}

const ACTIVITY_COLS: &str = "id, agent, session_id, repo_id, path, symbol_id, symbol_handle, \
                             kind, task, verdict, note, started_at, updated_at";

impl Store {
    /// Record that an agent is working on a path.
    ///
    /// An UPSERT on `(agent, repo_id, path)`, so a tool reporting every edit in
    /// a burst keeps one row that moves rather than accumulating a hundred.
    /// `started_at` survives the update — it is when this stretch of work
    /// began, which is what makes "editing for 4m" a sentence anyone can read.
    ///
    /// Emits `activity.started` only when the window actually opens (a new row,
    /// or a stale one coming back to life). A burst of edits to one file is one
    /// event, not one per callback — the event bus is the narrative, and
    /// "alice touched pay.ts" forty times in a row is not a narrative.
    pub fn record_activity(&mut self, a: &NewActivity) -> Result<Activity> {
        let a = a.clone();
        self.write(move |tx| {
            let now = ids::now_ms();

            // Was this window already open? Decides both `started_at` and
            // whether anyone needs to hear about it.
            let previous: Option<(i64, i64)> = tx
                .query_row(
                    "SELECT started_at, updated_at FROM activity \
                     WHERE agent = ?1 AND repo_id = ?2 AND path = ?3",
                    params![a.agent, a.repo_id, a.path],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .ok();
            let fresh = match previous {
                None => true,
                Some((_, updated)) => now - updated > ACTIVITY_LIVE_MS,
            };
            let started_at = match previous {
                Some((started, _)) if !fresh => started,
                _ => now,
            };

            tx.execute(
                "INSERT INTO activity(agent, session_id, repo_id, path, symbol_id, \
                 symbol_handle, kind, task, verdict, note, started_at, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12) \
                 ON CONFLICT(agent, repo_id, path) DO UPDATE SET \
                 session_id = ?2, symbol_id = ?5, symbol_handle = ?6, kind = ?7, \
                 task = ?8, verdict = ?9, note = ?10, started_at = ?11, updated_at = ?12",
                params![
                    a.agent,
                    a.session_id,
                    a.repo_id,
                    a.path,
                    a.symbol_id,
                    a.symbol_handle,
                    a.kind,
                    a.task,
                    a.verdict,
                    a.note,
                    started_at,
                    now,
                ],
            )?;

            if fresh {
                store::emit(
                    tx,
                    "activity.started",
                    Some(&a.agent),
                    a.symbol_handle.as_deref().or(Some(&a.path)),
                    a.task.as_deref(),
                    json!({
                        "path": a.path,
                        "kind": a.kind,
                        "verdict": a.verdict,
                        "repo": a.repo_id,
                    }),
                )?;
            }

            let row = tx.query_row(
                &format!(
                    "SELECT {ACTIVITY_COLS} FROM activity \
                     WHERE agent = ?1 AND repo_id = ?2 AND path = ?3"
                ),
                params![a.agent, a.repo_id, a.path],
                row_to_activity,
            )?;
            Ok(row)
        })
    }

    /// Close an agent's windows by hand — what a clean session end does.
    pub fn clear_activity(&mut self, agent: &str) -> Result<usize> {
        let agent = agent.to_string();
        self.write(move |tx| {
            let n = tx.execute("DELETE FROM activity WHERE agent = ?1", params![agent])?;
            if n > 0 {
                store::emit(
                    tx,
                    "activity.ended",
                    Some(&agent),
                    None,
                    None,
                    json!({ "cleared": n, "reason": "left" }),
                )?;
            }
            Ok(n)
        })
    }

    /// Everyone's open windows, most recently touched first.
    pub fn live_activity(&self) -> Result<Vec<ActivityView>> {
        let cutoff = ids::now_ms() - ACTIVITY_LIVE_MS;
        self.activity_where("updated_at >= ?1 ORDER BY updated_at DESC", params![cutoff])
    }

    /// Including windows that have gone quiet but not yet been swept — history
    /// is what tells a reader that someone *was* here.
    pub fn all_activity(&self, limit: usize) -> Result<Vec<ActivityView>> {
        self.activity_where(
            "1 = 1 ORDER BY updated_at DESC LIMIT ?1",
            params![limit as i64],
        )
    }

    pub fn activity_for_agent(&self, agent: &str) -> Result<Vec<ActivityView>> {
        self.activity_where("agent = ?1 ORDER BY updated_at DESC", params![agent])
    }

    /// Who is in this file. The direct answer to "should I start typing here?".
    pub fn activity_for_path(&self, repo_id: &str, path: &str) -> Result<Vec<ActivityView>> {
        self.activity_where(
            "repo_id = ?1 AND path = ?2 ORDER BY updated_at DESC",
            params![repo_id, path],
        )
    }

    /// Everything happening inside a symbol's subtree.
    ///
    /// Containment-aware in the same way leases are, and for the same reason:
    /// asking about a service has to cover its files, or a graph node would
    /// report itself quiet while three people worked inside it. Matches on the
    /// symbol itself, on any descendant symbol, and on the paths those cover —
    /// activity is recorded per path and may name no symbol at all.
    pub fn activity_under(&self, symbol_id: &str) -> Result<Vec<ActivityView>> {
        let mut ids: Vec<String> = vec![symbol_id.to_string()];
        let mut paths: Vec<String> = Vec::new();
        if let Some(root) = self.symbol(symbol_id)? {
            paths.push(root.path.clone());
        }
        for s in self.descendants(symbol_id)? {
            paths.push(s.path.clone());
            ids.push(s.id);
        }
        paths.sort();
        paths.dedup();

        let live = self.live_activity()?;
        Ok(live
            .into_iter()
            .filter(|v| {
                paths.contains(&v.activity.path)
                    || v.activity
                        .symbol_id
                        .as_ref()
                        .is_some_and(|s| ids.contains(s))
            })
            .collect())
    }

    /// Drop windows nobody has touched inside the live span.
    ///
    /// The lazy-expiry twin of `expire_due`, run from [`Store::sweep`]. Rows
    /// are deleted rather than tombstoned: the event log already records that
    /// the window opened, so the narrative survives without the table growing
    /// one row per file anyone ever opened.
    pub fn expire_activity(&mut self) -> Result<usize> {
        let cutoff = ids::now_ms() - ACTIVITY_LIVE_MS;
        let stale: Vec<(String, String)> = {
            let mut stmt = self
                .conn()
                .prepare("SELECT agent, path FROM activity WHERE updated_at < ?1")?;
            let rows = stmt.query_map(params![cutoff], |r| Ok((r.get(0)?, r.get(1)?)))?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        if stale.is_empty() {
            return Ok(0);
        }
        self.write(move |tx| {
            for (agent, path) in &stale {
                store::emit(
                    tx,
                    "activity.ended",
                    Some(agent),
                    Some(path),
                    None,
                    json!({ "path": path, "reason": "quiet" }),
                )?;
            }
            let n = tx.execute("DELETE FROM activity WHERE updated_at < ?1", params![cutoff])?;
            Ok(n)
        })
    }

    fn activity_where(&self, tail: &str, args: impl rusqlite::Params) -> Result<Vec<ActivityView>> {
        let mut stmt = self
            .conn()
            .prepare(&format!("SELECT {ACTIVITY_COLS} FROM activity WHERE {tail}"))?;
        let rows = stmt.query_map(args, row_to_activity)?;
        let now = ids::now_ms();
        Ok(rows
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .map(|a| ActivityView {
                live: now - a.updated_at <= ACTIVITY_LIVE_MS,
                age_secs: (now - a.updated_at) / 1000,
                activity: a,
            })
            .collect())
    }
}

fn row_to_activity(r: &Row) -> rusqlite::Result<Activity> {
    Ok(Activity {
        id: r.get(0)?,
        agent: r.get(1)?,
        session_id: r.get(2)?,
        repo_id: r.get(3)?,
        path: r.get(4)?,
        symbol_id: r.get(5)?,
        symbol_handle: r.get(6)?,
        kind: r.get(7)?,
        task: r.get(8)?,
        verdict: r.get(9)?,
        note: r.get(10)?,
        started_at: r.get(11)?,
        updated_at: r.get(12)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::DEFAULT_REPO_ID;

    fn fixture() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("src/pay.ts"),
            "export function charge(x: number) { return x; }\n\
             export function refund(x: number) { return x; }\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("src/ledger.ts"),
            "export function record(x: number) { return x; }\n",
        )
        .unwrap();
        let mut store = Store::init(dir.path()).unwrap();
        crate::scan::scan(&mut store, DEFAULT_REPO_ID, dir.path(), &[], false).unwrap();
        store.register_agent("alice", "cursor").unwrap();
        store.register_agent("bob", "claude-code").unwrap();
        (dir, store)
    }

    fn touch(store: &mut Store, agent: &str, path: &str, kind: &str) -> Activity {
        store
            .record_activity(&NewActivity::new(agent, DEFAULT_REPO_ID, path, kind))
            .unwrap()
    }

    /// Backdate a window past the live span without sleeping.
    fn go_quiet(store: &mut Store, agent: &str) {
        let stale = ids::now_ms() - ACTIVITY_LIVE_MS - 1_000;
        store
            .conn()
            .execute(
                "UPDATE activity SET updated_at = ?2 WHERE agent = ?1",
                params![agent, stale],
            )
            .unwrap();
    }

    #[test]
    fn a_burst_of_edits_is_one_row_that_moves() {
        let (_d, mut store) = fixture();
        let first = touch(&mut store, "alice", "src/pay.ts", kind::EDITING);
        for _ in 0..20 {
            touch(&mut store, "alice", "src/pay.ts", kind::EDITED);
        }

        let live = store.live_activity().unwrap();
        assert_eq!(live.len(), 1, "one row per agent per file, not one per edit");
        assert_eq!(live[0].activity.kind, kind::EDITED, "the latest kind wins");
        assert_eq!(
            live[0].activity.started_at, first.started_at,
            "the stretch of work began when it began"
        );
    }

    #[test]
    fn a_burst_of_edits_is_also_one_event() {
        let (_d, mut store) = fixture();
        for _ in 0..10 {
            touch(&mut store, "alice", "src/pay.ts", kind::EDITED);
        }
        let started = store
            .recent_events(200)
            .unwrap()
            .into_iter()
            .filter(|e| e.kind == "activity.started")
            .count();
        assert_eq!(started, 1, "the bus is a narrative, not a keylogger");
    }

    #[test]
    fn two_agents_in_two_files_are_two_windows() {
        let (_d, mut store) = fixture();
        touch(&mut store, "alice", "src/pay.ts", kind::EDITING);
        touch(&mut store, "bob", "src/ledger.ts", kind::EDITING);

        assert_eq!(store.live_activity().unwrap().len(), 2);
        assert_eq!(store.activity_for_agent("alice").unwrap().len(), 1);

        let here = store
            .activity_for_path(DEFAULT_REPO_ID, "src/pay.ts")
            .unwrap();
        assert_eq!(here.len(), 1);
        assert_eq!(here[0].activity.agent, "alice");
    }

    #[test]
    fn a_quiet_window_stops_being_live_and_then_is_swept() {
        let (_d, mut store) = fixture();
        touch(&mut store, "alice", "src/pay.ts", kind::EDITING);
        go_quiet(&mut store, "alice");

        assert!(
            store.live_activity().unwrap().is_empty(),
            "gone quiet is not still editing"
        );
        assert_eq!(store.all_activity(50).unwrap().len(), 1, "not yet swept");

        assert_eq!(store.expire_activity().unwrap(), 1);
        assert!(store.all_activity(50).unwrap().is_empty());
    }

    #[test]
    fn sweep_expires_activity_alongside_leases_and_sessions() {
        let (_d, mut store) = fixture();
        touch(&mut store, "alice", "src/pay.ts", kind::EDITING);
        go_quiet(&mut store, "alice");

        let report = store.sweep().unwrap();
        assert_eq!(
            report.activity, 1,
            "new expiry sweeps belong in sweep(), not in individual commands"
        );
    }

    #[test]
    fn coming_back_after_a_silence_starts_a_new_stretch() {
        let (_d, mut store) = fixture();
        let first = touch(&mut store, "alice", "src/pay.ts", kind::EDITING);
        go_quiet(&mut store, "alice");
        let second = touch(&mut store, "alice", "src/pay.ts", kind::EDITING);

        assert!(
            second.started_at > first.started_at,
            "a window that closed and reopened is a new stretch of work"
        );
        let started = store
            .recent_events(200)
            .unwrap()
            .into_iter()
            .filter(|e| e.kind == "activity.started")
            .count();
        assert_eq!(started, 2, "and it is worth announcing again");
    }

    #[test]
    fn activity_under_a_service_covers_its_files() {
        let (_d, mut store) = fixture();
        touch(&mut store, "alice", "src/pay.ts", kind::EDITING);

        // The file symbol itself, and the function inside it.
        let file = store.resolve("src/pay.ts").unwrap();
        assert_eq!(store.activity_under(&file.id).unwrap().len(), 1);

        // A sibling file's subtree knows nothing about it.
        let other = store.resolve("src/ledger.ts").unwrap();
        assert!(store.activity_under(&other.id).unwrap().is_empty());
    }

    #[test]
    fn a_symbol_named_by_id_is_found_under_its_ancestors() {
        let (_d, mut store) = fixture();
        let charge = store.resolve("charge").unwrap();
        store
            .record_activity(&NewActivity {
                symbol_id: Some(charge.id.clone()),
                symbol_handle: Some("src/pay.ts:charge".to_string()),
                ..NewActivity::new("alice", DEFAULT_REPO_ID, "src/pay.ts", kind::EDITED)
            })
            .unwrap();

        let file = store.resolve("src/pay.ts").unwrap();
        let under = store.activity_under(&file.id).unwrap();
        assert_eq!(under.len(), 1);
        assert_eq!(
            under[0].activity.symbol_handle.as_deref(),
            Some("src/pay.ts:charge"),
            "the file view names the exact symbol, not just the file"
        );
    }

    #[test]
    fn a_blocked_attempt_is_recorded_too() {
        let (_d, mut store) = fixture();
        store
            .record_activity(&NewActivity {
                verdict: Some("denied".to_string()),
                ..NewActivity::new("bob", DEFAULT_REPO_ID, "src/pay.ts", kind::BLOCKED)
            })
            .unwrap();

        let v = store.live_activity().unwrap();
        assert_eq!(v[0].activity.kind, kind::BLOCKED);
        assert_eq!(
            v[0].activity.verdict.as_deref(),
            Some("denied"),
            "a refused edit is exactly the contention a human wants to see"
        );
    }

    #[test]
    fn leaving_clears_what_you_were_editing() {
        let (_d, mut store) = fixture();
        touch(&mut store, "alice", "src/pay.ts", kind::EDITING);
        touch(&mut store, "alice", "src/ledger.ts", kind::EDITING);
        touch(&mut store, "bob", "src/pay.ts", kind::OPENED);

        assert_eq!(store.clear_activity("alice").unwrap(), 2);
        let left = store.live_activity().unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].activity.agent, "bob", "only the leaver is cleared");
    }
}
