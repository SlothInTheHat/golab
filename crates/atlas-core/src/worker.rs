//! Everyone doing work, human or not.
//!
//! Atlas treats humans and AI agents as the same kind of thing: *"Everything
//! performing work is a worker."* Underneath, the runtime already stores every
//! field a worker has — but spread across five tables, and three different
//! renderers were each deriving "is this one busy?" their own way and
//! disagreeing at the edges.
//!
//! This is that derivation, done once.
//!
//! # A view, not a table
//!
//! There is deliberately no `workers` table. A worker is a *join* over rows
//! that already exist and are already maintained:
//!
//! | field | comes from |
//! |---|---|
//! | identity, capability, paused | `agents` |
//! | tool, transport, uptime | `sessions` — which coding tool is attached |
//! | current file and symbol | `activity` — where their hands are |
//! | progress, ETA | `progress` |
//! | goal and task | `tasks` + `task_goals` |
//! | blocked | `lease::conflicts_for` on the task's scope |
//!
//! Storing it would mean maintaining a sixth copy that can disagree with the
//! five — the same reasoning that makes `throughput` a query over the event
//! log rather than a set of counters. This composes, like `context.rs` and
//! `notice.rs` before it.

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::activity::ActivityView;
use crate::ids;
use crate::model::*;
use crate::session::SessionView;
use crate::store::Store;
use crate::work::AGENT_ONLINE_MS;

/// What kind of thing is doing the work.
///
/// Atlas coordinates all three identically; the distinction exists so a human
/// reading the workspace can tell a colleague from their assistant at a
/// glance, which is most of what "multiplayer" means here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WorkerType {
    /// A person, at an editor.
    Human,
    /// A coding assistant: Claude Code, Cursor, Gemini, a local model.
    Ai,
    /// Long-running automation with no person behind it — CI, a docs bot.
    Service,
}

/// Tool slugs that identify a coding assistant rather than a person.
///
/// Matched as substrings, so `claude-code`, `claude` and `anthropic-claude`
/// all land in the same place. Free-form by design: an unrecognised tool is
/// still a worker, it just does not get an opinion attached.
const AI_TOOLS: &[&str] = &[
    "claude", "cursor", "copilot", "gemini", "opencode", "codex", "windsurf", "aider", "cody",
    "continue", "zed",
];

/// ...and the ones that are neither a person nor an assistant.
const SERVICE_TOOLS: &[&str] = &["ci", "bot", "agent", "runner", "worker", "pipeline"];

/// Human on the left, machine on the right.
///
/// `transport: "ide"` is the strongest signal there is — an editor extension
/// only exists because a person opened an editor — so it wins over whatever
/// the `kind` string says. Otherwise the tool name decides, and anything
/// unrecognised is assumed to be a person, because guessing "robot" about a
/// colleague is the worse mistake.
fn classify(kind: &str, transport: Option<&str>, tool: Option<&str>) -> WorkerType {
    if transport == Some(crate::session::transport::IDE) {
        return WorkerType::Human;
    }
    let needle = tool.unwrap_or(kind).to_ascii_lowercase();
    let k = kind.to_ascii_lowercase();
    if k == "human" || k == "person" || k == "dev" {
        return WorkerType::Human;
    }
    if AI_TOOLS.iter().any(|t| needle.contains(t) || k.contains(t)) {
        return WorkerType::Ai;
    }
    if SERVICE_TOOLS.iter().any(|t| needle.contains(t) || k.contains(t)) {
        return WorkerType::Service;
    }
    WorkerType::Human
}

/// What a worker is doing, in the order a reader cares about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WorkerStatus {
    /// Has work and can proceed.
    Working,
    /// Has submitted work and is waiting on somebody to look at it.
    Reviewing,
    /// Has work it cannot start, because somebody else holds what it needs.
    Blocked,
    /// Here, and available.
    Idle,
    /// Has not checked in inside [`AGENT_ONLINE_MS`].
    Offline,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Worker {
    pub name: String,
    #[serde(rename = "type")]
    pub worker_type: WorkerType,
    pub status: WorkerStatus,
    /// The `kind` the agent registered under, kept for display.
    pub kind: String,
    /// The coding tool actually attached right now, if one is.
    pub tool: Option<String>,
    /// `mcp` | `hook` | `ide` | `cli`, from the live session.
    pub transport: Option<String>,

    pub goal: Option<String>,
    pub task: Option<String>,
    pub task_title: Option<String>,
    pub percent: Option<i64>,
    pub eta_secs: Option<i64>,
    pub note: Option<String>,

    /// Where their hands are, from [`crate::activity`].
    pub current_file: Option<String>,
    pub current_symbol: Option<String>,

    pub capability: Option<Capability>,
    pub leases: usize,
    pub paused: bool,
    /// Who holds the thing this worker is waiting on. Only set when `Blocked`,
    /// and the reason a human can act on it rather than just seeing red.
    pub blocked_by: Option<String>,

    pub last_heartbeat: i64,
    /// Seconds since that heartbeat.
    pub silent_for: i64,
    pub joined_at: i64,
}

impl Worker {
    pub fn online(&self) -> bool {
        self.status != WorkerStatus::Offline
    }
    /// A tool is attached — as opposed to something merely heartbeating.
    pub fn connected(&self) -> bool {
        self.tool.is_some()
    }
}

impl Store {
    /// Everyone in the workspace, human and machine, with what they are doing.
    ///
    /// One pass over the five sources rather than a query per worker, because
    /// this is on the websocket snapshot and runs on every tick.
    pub fn workers(&self) -> Result<Vec<Worker>> {
        let agents = self.agents()?;
        let sessions: Vec<SessionView> = self.sessions(true)?;
        let activity: Vec<ActivityView> = self.live_activity()?;
        let progress = self.latest_progress()?;
        let tasks = self.tasks()?;
        let now = ids::now_ms();

        let mut out = Vec::new();
        for a in agents {
            let name = a.agent.name.clone();

            // The newest live session wins: an agent running both an MCP
            // server and an editor extension is a person with an assistant,
            // and `sessions(true)` is already newest-first.
            let session = sessions.iter().find(|s| s.session.agent == name);
            let tool = session.map(|s| s.session.tool.clone());
            let transport = session.map(|s| s.session.transport.clone());

            let act = activity.iter().find(|v| v.activity.agent == name);
            let prog = progress.iter().find(|p| p.agent == name);

            // Assignment, not leases. `AgentView::current_task` reads the
            // task off whatever lease the agent holds, which silently loses
            // the worker the moment somebody preempts them — they still own
            // the task, they simply cannot proceed, and that is precisely the
            // state worth showing. Live work first, then whatever is waiting.
            let task = open_task_of(&tasks, &name);
            let goal = match &task {
                Some(t) => self.task_goal(&t.task.id)?,
                None => None,
            };

            let (status, blocked_by) = self.status_of(&a, task, act.is_some())?;

            out.push(Worker {
                worker_type: classify(&a.agent.kind, transport.as_deref(), tool.as_deref()),
                status,
                kind: a.agent.kind.clone(),
                tool,
                transport,
                goal,
                task: task.map(|t| t.task.id.clone()),
                task_title: task.map(|t| t.task.title.clone()),
                percent: prog.and_then(|p| p.percent),
                eta_secs: prog.and_then(|p| p.eta_secs),
                note: prog.and_then(|p| p.note.clone()),
                current_file: act.map(|v| v.activity.path.clone()),
                current_symbol: act.and_then(|v| v.activity.symbol_handle.clone()),
                capability: a.agent.capability,
                leases: a.leases,
                paused: a.agent.paused,
                blocked_by,
                last_heartbeat: a.agent.heartbeat_at,
                silent_for: (now - a.agent.heartbeat_at) / 1000,
                joined_at: a.agent.joined_at,
                name,
            });
        }

        // Busy first, offline last: the order somebody scanning the list
        // actually wants, rather than whatever the agents table returns.
        out.sort_by_key(|w| (rank(w.status), w.name.clone()));
        Ok(out)
    }

    pub fn worker(&self, name: &str) -> Result<Option<Worker>> {
        Ok(self.workers()?.into_iter().find(|w| w.name == name))
    }

    /// The one derivation that is not a lookup.
    ///
    /// `Blocked` is what a scheduler already knows but nothing ever showed a
    /// human: this worker has been given a task whose scope somebody else is
    /// holding, so it cannot start however online and willing it is. Asking
    /// `conflicts_for` — the same function `acquire` uses — means the answer
    /// cannot drift from what the lease layer would actually do.
    fn status_of(
        &self,
        a: &crate::work::AgentView,
        task: Option<&crate::work::TaskView>,
        editing: bool,
    ) -> Result<(WorkerStatus, Option<String>)> {
        if !a.online {
            return Ok((WorkerStatus::Offline, None));
        }
        let Some(task) = task else {
            // No task, but hands on the keyboard, is still working — a human
            // exploring the code before claiming anything.
            return Ok((
                if editing {
                    WorkerStatus::Working
                } else {
                    WorkerStatus::Idle
                },
                None,
            ));
        };

        if task.task.state == TaskState::Review {
            return Ok((WorkerStatus::Reviewing, None));
        }

        for sym in self.task_scope(&task.task.id)? {
            if let Some(c) = self.conflicts_for(&sym.id, &a.agent.name)?.first() {
                return Ok((WorkerStatus::Blocked, Some(c.holder.clone())));
            }
        }
        if task.task.state == TaskState::Blocked {
            return Ok((
                WorkerStatus::Blocked,
                task.blocked_by.first().cloned(),
            ));
        }
        Ok((WorkerStatus::Working, None))
    }
}

/// The task this worker is on, by assignment.
///
/// A worker can legitimately hold several; the one that matters is the one
/// they are furthest into, so running beats submitted beats blocked beats
/// merely queued. Finished work is not a current task at all.
fn open_task_of<'a>(tasks: &'a [crate::work::TaskView], name: &str) -> Option<&'a crate::work::TaskView> {
    let priority = |s: TaskState| match s {
        TaskState::Running => 0,
        TaskState::Review => 1,
        TaskState::Blocked => 2,
        TaskState::Pending => 3,
        TaskState::Done | TaskState::Failed => 9,
    };
    tasks
        .iter()
        .filter(|t| t.task.assignee.as_deref() == Some(name))
        .filter(|t| priority(t.task.state) < 9)
        .min_by_key(|t| (priority(t.task.state), std::cmp::Reverse(t.task.priority)))
}

fn rank(s: WorkerStatus) -> u8 {
    match s {
        WorkerStatus::Working => 0,
        WorkerStatus::Blocked => 1,
        WorkerStatus::Reviewing => 2,
        WorkerStatus::Idle => 3,
        WorkerStatus::Offline => 4,
    }
}

/// How long an agent may be silent before it stops counting as present.
/// Re-exported so a caller does not have to reach into `work`.
pub const PRESENCE_WINDOW_MS: i64 = AGENT_ONLINE_MS;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::activity::{kind as akind, NewActivity};
    use crate::ids::DEFAULT_REPO_ID;
    use crate::lease::AcquireOptions;
    use crate::session::{transport, NewSession};

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
        crate::scan::scan(&mut store, DEFAULT_REPO_ID, dir.path(), &[], false).unwrap();
        (dir, store)
    }

    fn find<'a>(ws: &'a [Worker], name: &str) -> &'a Worker {
        ws.iter()
            .find(|w| w.name == name)
            .unwrap_or_else(|| panic!("no worker {name}"))
    }

    #[test]
    fn a_person_and_their_assistant_are_told_apart() {
        let (_d, mut store) = fixture();
        store.register_agent("alice", "human").unwrap();
        store.register_agent("claude-1", "claude-code").unwrap();
        store.register_agent("ci", "ci-runner").unwrap();

        let ws = store.workers().unwrap();
        assert_eq!(find(&ws, "alice").worker_type, WorkerType::Human);
        assert_eq!(find(&ws, "claude-1").worker_type, WorkerType::Ai);
        assert_eq!(find(&ws, "ci").worker_type, WorkerType::Service);
    }

    #[test]
    fn an_attached_editor_makes_a_worker_human_whatever_it_registered_as() {
        let (_d, mut store) = fixture();
        store.register_agent("bob", "cursor").unwrap();
        let ws = store.workers().unwrap();
        assert_eq!(
            find(&ws, "bob").worker_type,
            WorkerType::Ai,
            "a bare `cursor` registration is the assistant"
        );

        store
            .open_session(&NewSession::new("bob", "vscode", transport::IDE, "/repo"))
            .unwrap();
        let ws = store.workers().unwrap();
        assert_eq!(
            find(&ws, "bob").worker_type,
            WorkerType::Human,
            "an editor extension only exists because a person opened an editor"
        );
    }

    #[test]
    fn an_unrecognised_tool_is_assumed_to_be_a_person() {
        let (_d, mut store) = fixture();
        store.register_agent("dana", "some-new-editor").unwrap();
        assert_eq!(
            find(&store.workers().unwrap(), "dana").worker_type,
            WorkerType::Human,
            "guessing robot about a colleague is the worse mistake"
        );
    }

    #[test]
    fn the_tool_actually_attached_is_reported() {
        let (_d, mut store) = fixture();
        store.register_agent("alice", "claude").unwrap();
        assert!(find(&store.workers().unwrap(), "alice").tool.is_none());

        store
            .open_session(&NewSession::new("alice", "claude-code", transport::MCP, "/r"))
            .unwrap();
        let w = store.workers().unwrap();
        let alice = find(&w, "alice");
        assert_eq!(alice.tool.as_deref(), Some("claude-code"));
        assert_eq!(alice.transport.as_deref(), Some("mcp"));
        assert!(alice.connected());
    }

    #[test]
    fn a_worker_holding_nothing_is_idle_and_one_editing_is_working() {
        let (_d, mut store) = fixture();
        store.register_agent("alice", "human").unwrap();
        assert_eq!(
            find(&store.workers().unwrap(), "alice").status,
            WorkerStatus::Idle
        );

        store
            .record_activity(&NewActivity::new(
                "alice",
                DEFAULT_REPO_ID,
                "src/pay.ts",
                akind::EDITING,
            ))
            .unwrap();
        let ws = store.workers().unwrap();
        assert_eq!(
            find(&ws, "alice").status,
            WorkerStatus::Working,
            "hands on the keyboard is working, task or no task"
        );
        assert_eq!(find(&ws, "alice").current_file.as_deref(), Some("src/pay.ts"));
    }

    /// Being blocked is not something you can be *assigned* into — the
    /// scheduler refuses to hand out a scope somebody else holds, which is the
    /// whole point of it. It is something that happens *to* you: bob has the
    /// task and the lease, and then a higher-priority claim takes the symbol
    /// out from under him. He is still assigned, still online, and can no
    /// longer proceed — the state nothing previously showed a human.
    #[test]
    fn a_worker_preempted_off_its_own_scope_reads_as_blocked() {
        let (_d, mut store) = fixture();
        store.register_agent("alice", "human").unwrap();
        store.register_agent("bob", "human").unwrap();

        let t = store.add_task("rework charge", 5, &[]).unwrap();
        store.set_task_scope(&t.id, &["charge".to_string()]).unwrap();
        store.reassign_task(&t.id, "bob", Some("test")).unwrap();
        assert_eq!(
            find(&store.workers().unwrap(), "bob").status,
            WorkerStatus::Working,
            "he starts out fine"
        );

        store
            .acquire_ref(
                "charge",
                "alice",
                &AcquireOptions {
                    preempt: true,
                    priority: 9,
                    ..Default::default()
                },
            )
            .unwrap();

        let ws = store.workers().unwrap();
        let bob = find(&ws, "bob");
        assert_eq!(bob.status, WorkerStatus::Blocked);
        assert_eq!(
            bob.blocked_by.as_deref(),
            Some("alice"),
            "red is not actionable; a name is"
        );
        // alice took the symbol but has no task of her own, so she is merely
        // idle. Bob, who is stuck, sorts above her — the list is ordered by
        // who needs attention, not alphabetically.
        assert!(
            ws.iter().position(|w| w.name == "bob") < ws.iter().position(|w| w.name == "alice"),
            "blocked outranks idle: {:?}",
            ws.iter().map(|w| (&w.name, w.status)).collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_task_the_scheduler_marked_blocked_reads_as_blocked_too() {
        let (_d, mut store) = fixture();
        store.register_agent("alice", "human").unwrap();
        let first = store.add_task("upstream", 5, &[]).unwrap();
        let second = store.add_task("downstream", 5, std::slice::from_ref(&first.id)).unwrap();
        store.reassign_task(&second.id, "alice", Some("test")).unwrap();
        store
            .set_task_state(&second.id, TaskState::Blocked, Some("alice"), None, true)
            .unwrap();

        let ws = store.workers().unwrap();
        assert_eq!(find(&ws, "alice").status, WorkerStatus::Blocked);
        assert_eq!(
            find(&ws, "alice").blocked_by.as_deref(),
            Some(first.id.as_str()),
            "a dependency is a blocker with a name too"
        );
    }

    #[test]
    fn submitted_work_reads_as_reviewing() {
        let (_d, mut store) = fixture();
        store.register_agent("alice", "human").unwrap();
        let t = store.add_task("charge", 5, &[]).unwrap();
        store.set_task_scope(&t.id, &["charge".to_string()]).unwrap();
        store.reassign_task(&t.id, "alice", Some("test")).unwrap();
        store.submit_for_review(&t.id, "alice").unwrap();

        assert_eq!(
            find(&store.workers().unwrap(), "alice").status,
            WorkerStatus::Reviewing
        );
    }

    #[test]
    fn a_silent_worker_goes_offline_and_sorts_last() {
        let (_d, mut store) = fixture();
        store.register_agent("alice", "human").unwrap();
        store.register_agent("ghost", "human").unwrap();
        store
            .conn()
            .execute(
                "UPDATE agents SET heartbeat_at = ?2 WHERE name = ?1",
                rusqlite::params!["ghost", ids::now_ms() - PRESENCE_WINDOW_MS - 5_000],
            )
            .unwrap();

        let ws = store.workers().unwrap();
        assert_eq!(find(&ws, "ghost").status, WorkerStatus::Offline);
        assert!(!find(&ws, "ghost").online());
        assert!(find(&ws, "ghost").silent_for >= 60);
        assert_eq!(
            ws.last().unwrap().name,
            "ghost",
            "busy first, gone last — the order a reader wants"
        );
    }

    #[test]
    fn progress_and_the_current_task_ride_along() {
        let (_d, mut store) = fixture();
        store.register_agent("alice", "claude").unwrap();
        let t = store.add_task("charge it", 5, &[]).unwrap();
        store.set_task_scope(&t.id, &["charge".to_string()]).unwrap();
        store.reassign_task(&t.id, "alice", Some("test")).unwrap();
        store
            .record_progress("alice", Some(&t.id), None, Some(60), Some(120), Some("halfway"))
            .unwrap();

        let ws = store.workers().unwrap();
        let alice = find(&ws, "alice");
        assert_eq!(alice.task.as_deref(), Some(t.id.as_str()));
        assert_eq!(alice.task_title.as_deref(), Some("charge it"));
        assert_eq!(alice.percent, Some(60));
        assert_eq!(alice.eta_secs, Some(120));
        assert_eq!(alice.note.as_deref(), Some("halfway"));
        assert_eq!(alice.leases, 1, "claiming a task leases its scope");
    }
}
