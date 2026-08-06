//! Shared workspace: who is here, what is being worked on, what everyone knows.
//!
//! This is the state that stops each agent from rediscovering the project on
//! every prompt — presence, a task graph with dependencies, shared memory, and
//! structured agent-to-agent messages.

use anyhow::{anyhow, Result};
use rusqlite::{params, OptionalExtension, Row};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::ids;
use crate::model::*;
use crate::store::{self, Store};

/// An agent that has not heartbeated within this window is presumed gone.
pub const AGENT_ONLINE_MS: i64 = 60_000;

/// What one `sweep` reclaimed. A struct rather than a tuple so a fourth kind
/// of lazy expiry can be added without touching every call site.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct SweepReport {
    pub leases: usize,
    pub requests: usize,
    pub sessions: usize,
    pub activity: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentView {
    #[serde(flatten)]
    pub agent: Agent,
    pub online: bool,
    pub leases: usize,
    pub current_task: Option<String>,
    /// The goal `current_task` serves, if it's scoped to one.
    pub current_goal: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskView {
    #[serde(flatten)]
    pub task: Task,
    /// Pending with every dependency satisfied: the scheduler can hand it out.
    pub ready: bool,
    pub blocked_by: Vec<String>,
}

/// What the repository looks like, as opposed to what the agents are doing.
#[derive(Debug, Clone, Serialize)]
pub struct KnowledgeSummary {
    pub services: i64,
    pub endpoints: i64,
    pub tables: i64,
    pub tests: i64,
    /// Endpoints as `METHOD /path`, for a dashboard that wants the list.
    pub routes: Vec<String>,
}

/// One-glance picture of the runtime.
#[derive(Debug, Clone, Serialize)]
pub struct StatusSummary {
    pub files: i64,
    pub symbols: i64,
    pub edges: i64,
    pub knowledge: KnowledgeSummary,
    pub agents: Vec<AgentView>,
    pub leases: Vec<Lease>,
    pub waiting: i64,
    pub tasks: Vec<TaskView>,
    /// Open negotiations between agents.
    pub requests: Vec<Request>,
    /// Latest progress report per agent.
    pub progress: Vec<ProgressUpdate>,
    /// The scheduler's headline: what could start, and how wide it can go.
    pub schedule: ScheduleSummary,
    pub recent_events: Vec<Event>,
}

/// What the swarm got done over a window, derived from the event log.
#[derive(Debug, Clone, Serialize)]
pub struct Throughput {
    pub window_minutes: i64,
    pub bucket_ms: i64,
    pub tasks_completed: i64,
    pub tasks_started: i64,
    pub tasks_reassigned: i64,
    pub leases_acquired: i64,
    /// Denials are the contention signal: high and rising means the work is
    /// carved up badly, not that the agents are slow.
    pub leases_denied: i64,
    pub leases_expired: i64,
    pub handovers: i64,
    pub agents_active: i64,
    /// Mean claim-to-completion time, or `None` if nothing finished.
    pub mean_task_secs: Option<f64>,
    /// Completions per bucket, oldest first.
    pub completed_series: Vec<i64>,
}

/// Enough of the plan for a dashboard, without the whole wave structure.
#[derive(Debug, Clone, Serialize)]
pub struct ScheduleSummary {
    pub startable_now: i64,
    pub max_parallel: i64,
    pub waves: i64,
    pub cycles: i64,
    /// Ready tasks that cannot start because their scope is held.
    pub contended: i64,
}

impl Store {
    // --------------------------------------------------------------- agents

    /// Announce an agent. Idempotent: re-registering just refreshes presence.
    pub fn register_agent(&mut self, name: &str, kind: &str) -> Result<Agent> {
        let name = name.to_string();
        let kind = kind.to_string();
        self.write(move |tx| {
            let now = ids::now_ms();
            let existed: bool = tx
                .query_row(
                    "SELECT 1 FROM agents WHERE name = ?1",
                    params![name],
                    |_| Ok(true),
                )
                .optional()?
                .unwrap_or(false);
            tx.execute(
                "INSERT INTO agents(name, kind, joined_at, heartbeat_at, status) \
                 VALUES (?1, ?2, ?3, ?3, 'idle') \
                 ON CONFLICT(name) DO UPDATE SET kind = excluded.kind, heartbeat_at = ?3",
                params![name, kind, now],
            )?;
            if !existed {
                store::emit(
                    tx,
                    "agent.joined",
                    Some(&name),
                    None,
                    None,
                    json!({ "kind": kind }),
                )?;
            }
            Ok(Agent {
                name,
                kind,
                joined_at: now,
                heartbeat_at: now,
                status: "idle".to_string(),
                paused: false,
                capability: None,
            })
        })
    }

    pub fn set_agent_status(&mut self, name: &str, status: &str) -> Result<()> {
        let name = name.to_string();
        let status = status.to_string();
        self.write(move |tx| {
            tx.execute(
                "UPDATE agents SET status = ?2, heartbeat_at = ?3 WHERE name = ?1",
                params![name, status, ids::now_ms()],
            )?;
            Ok(())
        })
    }

    /// Stop handing an agent new work, without taking away what it has.
    ///
    /// The pause button on the dashboard: useful when an agent is misbehaving
    /// and you want it to finish its current task and then stop, rather than
    /// killing it and waiting for TTLs to unwind.
    pub fn set_agent_paused(&mut self, name: &str, paused: bool) -> Result<Agent> {
        let name_owned = name.to_string();
        self.write(move |tx| {
            let n = tx.execute(
                "UPDATE agents SET paused = ?2 WHERE name = ?1",
                params![name_owned, i64::from(paused)],
            )?;
            if n == 0 {
                return Err(anyhow!("no such agent: {name_owned}"));
            }
            store::emit(
                tx,
                if paused { "agent.paused" } else { "agent.resumed" },
                Some(&name_owned),
                None,
                None,
                json!({}),
            )?;
            Ok(())
        })?;
        self.agent(name)?
            .ok_or_else(|| anyhow!("agent vanished: {name}"))
    }

    /// Declare (or clear) an agent's role. `None` makes it a generalist
    /// again, eligible for any task regardless of `required_capability`.
    pub fn set_agent_capability(&mut self, name: &str, capability: Option<Capability>) -> Result<Agent> {
        let name_owned = name.to_string();
        self.write(move |tx| {
            let n = tx.execute(
                "UPDATE agents SET capability = ?2 WHERE name = ?1",
                params![name_owned, capability.map(|c| c.as_str())],
            )?;
            if n == 0 {
                return Err(anyhow!("no such agent: {name_owned}"));
            }
            Ok(())
        })?;
        self.agent(name)?
            .ok_or_else(|| anyhow!("agent vanished: {name}"))
    }

    pub fn agent(&self, name: &str) -> Result<Option<Agent>> {
        Ok(self
            .conn()
            .query_row(
                "SELECT name, kind, joined_at, heartbeat_at, status, paused, capability FROM agents \
                 WHERE name = ?1",
                params![name],
                row_to_agent,
            )
            .optional()?)
    }

    /// Leave cleanly: hand back every lease instead of waiting for TTLs.
    pub fn deregister_agent(&mut self, name: &str) -> Result<usize> {
        let released = self.release_all(name)?.len();
        let name = name.to_string();
        self.write(move |tx| {
            tx.execute("DELETE FROM agents WHERE name = ?1", params![name])?;
            store::emit(
                tx,
                "agent.left",
                Some(&name),
                None,
                None,
                json!({ "leases_released": released }),
            )?;
            Ok(())
        })?;
        Ok(released)
    }

    pub fn agents(&self) -> Result<Vec<AgentView>> {
        let mut stmt = self
            .conn()
            .prepare(
                "SELECT name, kind, joined_at, heartbeat_at, status, paused, capability FROM agents \
                 ORDER BY name",
            )?;
        let agents: Vec<Agent> = stmt
            .query_map([], row_to_agent)?
            .collect::<Result<Vec<_>, _>>()?;
        let now = ids::now_ms();
        let leases = self.active_leases(None)?;
        let mut out = Vec::with_capacity(agents.len());
        for a in agents {
            let mine: Vec<&Lease> = leases.iter().filter(|l| l.agent == a.name).collect();
            let current_task = mine.iter().find_map(|l| l.task.clone());
            let current_goal = match &current_task {
                Some(t) => self.task_goal(t)?,
                None => None,
            };
            out.push(AgentView {
                online: now - a.heartbeat_at <= AGENT_ONLINE_MS,
                leases: mine.len(),
                current_task,
                current_goal,
                agent: a,
            });
        }
        Ok(out)
    }

    // ---------------------------------------------------------------- tasks

    /// Add a task, optionally depending on others.
    pub fn add_task(&mut self, title: &str, priority: i64, deps: &[String]) -> Result<Task> {
        let title = title.to_string();
        let deps: Vec<String> = deps.to_vec();
        self.write(move |tx| {
            let n: i64 = tx.query_row("SELECT COUNT(*) FROM tasks", [], |r| r.get(0))?;
            let id = format!("T{}", n + 1);
            let now = ids::now_ms();
            tx.execute(
                "INSERT INTO tasks(id, title, state, priority, assignee, created_at, updated_at) \
                 VALUES (?1, ?2, 'pending', ?3, NULL, ?4, ?4)",
                params![id, title, priority, now],
            )?;
            for d in &deps {
                let exists: bool = tx
                    .query_row("SELECT 1 FROM tasks WHERE id = ?1", params![d], |_| Ok(true))
                    .optional()?
                    .unwrap_or(false);
                if !exists {
                    return Err(anyhow!("unknown dependency: {d}"));
                }
                tx.execute(
                    "INSERT OR IGNORE INTO task_deps(task_id, depends_on) VALUES (?1, ?2)",
                    params![id, d],
                )?;
            }
            store::emit(
                tx,
                "task.created",
                None,
                None,
                Some(&id),
                json!({ "title": title, "priority": priority, "deps": deps }),
            )?;
            Ok(Task {
                id,
                title,
                state: TaskState::Pending,
                priority,
                assignee: None,
                deps,
                created_at: now,
                updated_at: now,
                required_capability: None,
            })
        })
    }

    pub fn tasks(&self) -> Result<Vec<TaskView>> {
        let mut stmt = self.conn().prepare(
            "SELECT id, title, state, priority, assignee, created_at, updated_at, required_capability \
             FROM tasks ORDER BY priority DESC, created_at",
        )?;
        let tasks: Vec<Task> = stmt
            .query_map([], row_to_task)?
            .collect::<Result<Vec<_>, _>>()?;

        let mut dep_stmt = self
            .conn()
            .prepare("SELECT depends_on FROM task_deps WHERE task_id = ?1")?;
        let done: std::collections::HashSet<String> = tasks
            .iter()
            .filter(|t| t.state == TaskState::Done)
            .map(|t| t.id.clone())
            .collect();

        let mut out = Vec::with_capacity(tasks.len());
        for mut t in tasks {
            let deps: Vec<String> = dep_stmt
                .query_map(params![t.id], |r| r.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            let blocked_by: Vec<String> =
                deps.iter().filter(|d| !done.contains(*d)).cloned().collect();
            t.deps = deps;
            out.push(TaskView {
                ready: t.state == TaskState::Pending && blocked_by.is_empty(),
                blocked_by,
                task: t,
            });
        }
        Ok(out)
    }

    pub fn task(&self, id: &str) -> Result<Option<TaskView>> {
        Ok(self.tasks()?.into_iter().find(|t| t.task.id == id))
    }

    /// Tasks the scheduler could hand out right now, best first.
    pub fn ready_tasks(&self) -> Result<Vec<TaskView>> {
        Ok(self.tasks()?.into_iter().filter(|t| t.ready).collect())
    }

    /// Claim the highest-priority task this agent can safely start.
    ///
    /// Thin wrapper over [`Store::claim_next`], which is the real scheduler:
    /// it skips tasks whose scope is held by someone else and leases the scope
    /// it hands out. Kept for callers that only want the task back.
    pub fn claim_next_task(&mut self, agent: &str) -> Result<Option<TaskView>> {
        Ok(self
            .claim_next(agent, crate::lease::DEFAULT_TTL_SECS)?
            .map(|s| s.task))
    }

    /// Move a task to a new state.
    ///
    /// Closing work belongs to whoever holds it: an agent cannot mark another
    /// agent's task done or failed, because that agent is probably still
    /// editing the symbols the task reserved. `force` is the deliberate
    /// override for a human cleaning up after a crash.
    pub fn set_task_state(
        &mut self,
        id: &str,
        state: TaskState,
        agent: Option<&str>,
        note: Option<&str>,
        force: bool,
    ) -> Result<TaskView> {
        let terminal = matches!(state, TaskState::Done | TaskState::Failed);
        if terminal && !force {
            if let (Some(current), Some(actor)) = (
                self.task(id)?.and_then(|t| t.task.assignee),
                agent.map(|a| a.to_string()),
            ) {
                if current != actor {
                    return Err(anyhow!(
                        "{id} is assigned to {current}, not {actor} — pass --force to override"
                    ));
                }
            }
        }
        let id_owned = id.to_string();
        let agent_owned = agent.map(|s| s.to_string());
        let note_owned = note.map(|s| s.to_string());
        self.write(move |tx| {
            let exists: bool = tx
                .query_row("SELECT 1 FROM tasks WHERE id = ?1", params![id_owned], |_| {
                    Ok(true)
                })
                .optional()?
                .unwrap_or(false);
            if !exists {
                return Err(anyhow!("no such task: {id_owned}"));
            }
            tx.execute(
                "UPDATE tasks SET state = ?2, updated_at = ?3 WHERE id = ?1",
                params![id_owned, state.as_str(), ids::now_ms()],
            )?;
            if terminal {
                // The work is over, so the reservations it was handed are too.
                // Leaving them to time out would idle the next agent for a
                // whole TTL over work that is already finished.
                crate::lease::release_task_leases(tx, &id_owned)?;
            }
            if state == TaskState::Done {
                // Anyone blocked on this task is now unblocked; tell them
                // rather than making them poll the task list.
                crate::protocol::resolve_task_requests(tx, &id_owned, "task completed")?;
            }
            let kind = match state {
                TaskState::Done => "task.completed",
                TaskState::Blocked => "task.blocked",
                TaskState::Failed => "task.failed",
                TaskState::Running => "task.started",
                TaskState::Review => "task.submitted_for_review",
                TaskState::Pending => "task.reset",
            };
            store::emit(
                tx,
                kind,
                agent_owned.as_deref(),
                None,
                Some(&id_owned),
                json!({ "note": note_owned }),
            )?;
            Ok(())
        })?;

        // Completing a task can unblock others; say so on the bus.
        if state == TaskState::Done {
            let unblocked: Vec<String> = self
                .tasks()?
                .into_iter()
                .filter(|t| t.ready && t.task.deps.iter().any(|d| d == id))
                .map(|t| t.task.id)
                .collect();
            if !unblocked.is_empty() {
                let id_owned = id.to_string();
                self.write(move |tx| {
                    for u in &unblocked {
                        store::emit(
                            tx,
                            "task.unblocked",
                            None,
                            None,
                            Some(u),
                            json!({ "by": id_owned }),
                        )?;
                    }
                    Ok(())
                })?;
            }
        }

        self.task(id)?
            .ok_or_else(|| anyhow!("task vanished: {id}"))
    }

    // --------------------------------------------------------------- memory

    pub fn memory_set(
        &mut self,
        key: &str,
        value: &str,
        author: Option<&str>,
        tags: &[String],
    ) -> Result<()> {
        let key = key.to_string();
        let value = value.to_string();
        let author = author.map(|s| s.to_string());
        let tags = tags.join(",");
        self.write(move |tx| {
            tx.execute(
                "INSERT INTO memory(key, value, author, tags, updated_at) VALUES (?1, ?2, ?3, ?4, ?5) \
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value, author = excluded.author, \
                     tags = excluded.tags, updated_at = excluded.updated_at",
                params![key, value, author, tags, ids::now_ms()],
            )?;
            store::emit(
                tx,
                "memory.written",
                author.as_deref(),
                None,
                None,
                json!({ "key": key }),
            )?;
            Ok(())
        })
    }

    pub fn memory_get(&self, key: &str) -> Result<Option<MemoryEntry>> {
        Ok(self
            .conn()
            .query_row(
                "SELECT key, value, author, tags, updated_at FROM memory WHERE key = ?1",
                params![key],
                row_to_memory,
            )
            .optional()?)
    }

    pub fn memory_list(&self, tag: Option<&str>) -> Result<Vec<MemoryEntry>> {
        let mut stmt = self
            .conn()
            .prepare("SELECT key, value, author, tags, updated_at FROM memory ORDER BY key")?;
        let all: Vec<MemoryEntry> = stmt
            .query_map([], row_to_memory)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(match tag {
            Some(t) => all
                .into_iter()
                .filter(|m| m.tags.iter().any(|x| x == t))
                .collect(),
            None => all,
        })
    }

    pub fn memory_delete(&mut self, key: &str) -> Result<bool> {
        let key = key.to_string();
        self.write(move |tx| {
            let n = tx.execute("DELETE FROM memory WHERE key = ?1", params![key])?;
            Ok(n > 0)
        })
    }

    // ------------------------------------------------------------- messages

    /// Structured request/response between agents, not free-form chat.
    pub fn send_message(
        &mut self,
        from: &str,
        to: &str,
        subject: &str,
        body: serde_json::Value,
        reply_to: Option<i64>,
    ) -> Result<i64> {
        let from = from.to_string();
        let to = to.to_string();
        let subject = subject.to_string();
        self.write(move |tx| {
            tx.execute(
                "INSERT INTO messages(from_agent, to_agent, subject, body, sent_at, read_at, reply_to) \
                 VALUES (?1, ?2, ?3, ?4, ?5, NULL, ?6)",
                params![from, to, subject, body.to_string(), ids::now_ms(), reply_to],
            )?;
            let id = tx.last_insert_rowid();
            store::emit(
                tx,
                "message.sent",
                Some(&from),
                None,
                None,
                json!({ "id": id, "to": to, "subject": subject }),
            )?;
            Ok(id)
        })
    }

    pub fn inbox(&self, agent: &str, unread_only: bool) -> Result<Vec<Message>> {
        let sql = if unread_only {
            "SELECT id, from_agent, to_agent, subject, body, sent_at, read_at, reply_to \
             FROM messages WHERE to_agent = ?1 AND read_at IS NULL ORDER BY id"
        } else {
            "SELECT id, from_agent, to_agent, subject, body, sent_at, read_at, reply_to \
             FROM messages WHERE to_agent = ?1 ORDER BY id"
        };
        let mut stmt = self.conn().prepare(sql)?;
        let rows = stmt.query_map(params![agent], row_to_message)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    pub fn mark_read(&mut self, id: i64, agent: &str) -> Result<bool> {
        let agent = agent.to_string();
        self.write(move |tx| {
            let n = tx.execute(
                "UPDATE messages SET read_at = ?3 WHERE id = ?1 AND to_agent = ?2 AND read_at IS NULL",
                params![id, agent, ids::now_ms()],
            )?;
            Ok(n > 0)
        })
    }

    // --------------------------------------------------------------- status

    pub fn status(&self, event_limit: usize) -> Result<StatusSummary> {
        Ok(StatusSummary {
            files: self.file_count()?,
            symbols: self.symbol_count()?,
            edges: self.edge_count()?,
            knowledge: self.knowledge()?,
            agents: self.agents()?,
            leases: self.active_leases(None)?,
            waiting: self.waiting_count()?,
            tasks: self.tasks()?,
            requests: self.requests(None, crate::protocol::Direction::All, true)?,
            progress: self.latest_progress()?,
            schedule: self.schedule_summary()?,
            recent_events: self.recent_events(event_limit)?,
        })
    }

    /// What the swarm actually got done, from the event log.
    ///
    /// The log is already the single narrative of the runtime, so throughput
    /// is a query over it rather than a second set of counters that could
    /// disagree with it.
    pub fn throughput(&self, minutes: i64, buckets: usize) -> Result<Throughput> {
        let now = ids::now_ms();
        let window_ms = minutes.max(1) * 60_000;
        let since = now - window_ms;
        let buckets = buckets.clamp(1, 240);
        let bucket_ms = (window_ms / buckets as i64).max(1);

        let count = |kind: &str| -> Result<i64> {
            Ok(self.conn().query_row(
                "SELECT COUNT(*) FROM events WHERE kind = ?1 AND ts >= ?2",
                params![kind, since],
                |r| r.get(0),
            )?)
        };

        // Completions per bucket, oldest first — a sparkline for the dashboard.
        let mut completed_series = vec![0i64; buckets];
        let mut stmt = self
            .conn()
            .prepare("SELECT ts FROM events WHERE kind = 'task.completed' AND ts >= ?1")?;
        for ts in stmt.query_map(params![since], |r| r.get::<_, i64>(0))? {
            let idx = (((ts? - since) / bucket_ms) as usize).min(buckets - 1);
            completed_series[idx] += 1;
        }
        drop(stmt);

        // Mean wall-clock time from claim to completion.
        let mean_task_secs: Option<f64> = self
            .conn()
            .query_row(
                "SELECT AVG(updated_at - claimed_at) / 1000.0 FROM tasks \
                 WHERE state = 'done' AND claimed_at IS NOT NULL AND updated_at >= ?1",
                params![since],
                |r| r.get(0),
            )
            .optional()?
            .flatten();

        let agents_active: i64 = self.conn().query_row(
            "SELECT COUNT(DISTINCT agent) FROM events WHERE agent IS NOT NULL AND ts >= ?1",
            params![since],
            |r| r.get(0),
        )?;

        Ok(Throughput {
            window_minutes: minutes,
            bucket_ms,
            tasks_completed: count("task.completed")?,
            tasks_started: count("task.started")?,
            tasks_reassigned: count("task.reassigned")?,
            leases_acquired: count("lease.acquired")?,
            leases_denied: count("lease.denied")?,
            leases_expired: count("lease.expired")?,
            handovers: count("lease.transferred")?,
            agents_active,
            mean_task_secs,
            completed_series,
        })
    }

    /// The scheduler headline for `status` and the dashboard.
    pub fn schedule_summary(&self) -> Result<ScheduleSummary> {
        let plan = self.plan()?;
        let contended = plan
            .waves
            .first()
            .map(|w| w.tasks.iter().filter(|t| t.contended_by.is_some()).count())
            .unwrap_or(0);
        Ok(ScheduleSummary {
            startable_now: plan.startable_now as i64,
            max_parallel: plan.max_parallel as i64,
            waves: plan.waves.len() as i64,
            cycles: plan.cycles.len() as i64,
            contended: contended as i64,
        })
    }

    /// Counts and routes from the knowledge graph.
    pub fn knowledge(&self) -> Result<KnowledgeSummary> {
        let count = |sql: &str| -> Result<i64> {
            Ok(self.conn().query_row(sql, [], |r| r.get(0))?)
        };
        let endpoints = self.symbols_with_role(Role::Api)?;
        Ok(KnowledgeSummary {
            services: count("SELECT COUNT(*) FROM symbols WHERE kind = 'service'")?,
            endpoints: endpoints.len() as i64,
            tables: count("SELECT COUNT(*) FROM symbols WHERE kind = 'table'")?,
            tests: count("SELECT COUNT(*) FROM symbols WHERE role = 'test'")?,
            routes: endpoints.iter().filter_map(|e| e.route()).collect(),
        })
    }

    /// One call for every lazy expiry in the runtime: leases whose TTL ran out,
    /// requests whose deadline passed, and connections that went quiet.
    /// Watchers and the daemon tick this.
    pub fn sweep(&mut self) -> Result<SweepReport> {
        let leases = self.expire_due()?;
        let requests = self.expire_requests()?;
        // A coding tool killed outright never closes its stdin, so nothing
        // announces its departure. Reaping it here is what keeps "who is
        // attached" honest without anyone having to notice the crash.
        let sessions = self.expire_sessions()?;
        // An edit window nobody has touched is over, whether or not anyone
        // announced it — same reasoning as the session above, one layer down.
        let activity = self.expire_activity()?;
        // Work abandoned by a dead agent is the last thing that has to
        // self-heal, alongside its leases and its unanswered requests.
        self.reassign_orphans()?;
        Ok(SweepReport {
            leases,
            requests,
            sessions,
            activity,
        })
    }
}

fn row_to_agent(r: &Row) -> rusqlite::Result<Agent> {
    Ok(Agent {
        name: r.get(0)?,
        kind: r.get(1)?,
        joined_at: r.get(2)?,
        heartbeat_at: r.get(3)?,
        status: r.get(4)?,
        paused: r.get::<_, i64>(5).unwrap_or(0) != 0,
        capability: r.get::<_, Option<String>>(6)?.and_then(|s| Capability::parse(&s)),
    })
}

fn row_to_task(r: &Row) -> rusqlite::Result<Task> {
    let state: String = r.get(2)?;
    Ok(Task {
        id: r.get(0)?,
        title: r.get(1)?,
        state: TaskState::parse(&state).unwrap_or(TaskState::Pending),
        priority: r.get(3)?,
        assignee: r.get(4)?,
        deps: Vec::new(),
        created_at: r.get(5)?,
        updated_at: r.get(6)?,
        required_capability: r.get::<_, Option<String>>(7)?.and_then(|s| Capability::parse(&s)),
    })
}

fn row_to_memory(r: &Row) -> rusqlite::Result<MemoryEntry> {
    let tags: String = r.get(3)?;
    Ok(MemoryEntry {
        key: r.get(0)?,
        value: r.get(1)?,
        author: r.get(2)?,
        tags: tags
            .split(',')
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .collect(),
        updated_at: r.get(4)?,
    })
}

fn row_to_message(r: &Row) -> rusqlite::Result<Message> {
    let body: String = r.get(4)?;
    Ok(Message {
        id: r.get(0)?,
        from: r.get(1)?,
        to: r.get(2)?,
        subject: r.get(3)?,
        body: serde_json::from_str(&body).unwrap_or_else(|_| json!({})),
        sent_at: r.get(5)?,
        read_at: r.get(6)?,
        reply_to: r.get(7)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agents_report_presence_and_held_leases() {
        let mut s = Store::open_in_memory().unwrap();
        s.register_agent("claude-1", "claude").unwrap();
        s.register_agent("cursor-1", "cursor").unwrap();
        let agents = s.agents().unwrap();
        assert_eq!(agents.len(), 2);
        assert!(agents.iter().all(|a| a.online));
        assert!(agents.iter().all(|a| a.leases == 0));

        // Registering twice must not duplicate the join event.
        s.register_agent("claude-1", "claude").unwrap();
        let joins = s
            .recent_events(50)
            .unwrap()
            .into_iter()
            .filter(|e| e.kind == "agent.joined")
            .count();
        assert_eq!(joins, 2);
    }

    #[test]
    fn task_graph_orders_work_by_dependency_then_priority() {
        let mut s = Store::open_in_memory().unwrap();
        let backend = s.add_task("backend auth", 5, &[]).unwrap();
        let frontend = s
            .add_task("frontend login", 9, std::slice::from_ref(&backend.id))
            .unwrap();
        let docs = s.add_task("docs", 1, &[]).unwrap();

        // Frontend outranks everything but depends on backend, so it waits.
        let ready: Vec<String> = s
            .ready_tasks()
            .unwrap()
            .into_iter()
            .map(|t| t.task.id)
            .collect();
        assert_eq!(ready, vec![backend.id.clone(), docs.id.clone()]);

        let claimed = s.claim_next_task("agent-1").unwrap().unwrap();
        assert_eq!(claimed.task.id, backend.id);
        assert_eq!(claimed.task.assignee.as_deref(), Some("agent-1"));

        // A second agent gets the next unblocked task, not the same one.
        let second = s.claim_next_task("agent-2").unwrap().unwrap();
        assert_eq!(second.task.id, docs.id);

        s.set_task_state(&backend.id, TaskState::Done, Some("agent-1"), None, false)
            .unwrap();
        let now_ready: Vec<String> = s
            .ready_tasks()
            .unwrap()
            .into_iter()
            .map(|t| t.task.id)
            .collect();
        assert_eq!(now_ready, vec![frontend.id.clone()]);

        let kinds: Vec<String> = s
            .recent_events(50)
            .unwrap()
            .into_iter()
            .map(|e| e.kind)
            .collect();
        assert!(kinds.contains(&"task.unblocked".to_string()), "{kinds:?}");
    }

    #[test]
    fn unknown_dependencies_are_rejected() {
        let mut s = Store::open_in_memory().unwrap();
        assert!(s.add_task("x", 0, &["T99".to_string()]).is_err());
        assert!(s.tasks().unwrap().is_empty(), "failed insert must roll back");
    }

    #[test]
    fn shared_memory_round_trips_with_tags() {
        let mut s = Store::open_in_memory().unwrap();
        s.memory_set(
            "decision/auth",
            "sessions are JWT, 15 min expiry",
            Some("claude-1"),
            &["architecture".to_string()],
        )
        .unwrap();
        s.memory_set("glossary/lease", "time-bounded ownership", None, &[])
            .unwrap();

        let entry = s.memory_get("decision/auth").unwrap().unwrap();
        assert_eq!(entry.author.as_deref(), Some("claude-1"));
        assert_eq!(s.memory_list(Some("architecture")).unwrap().len(), 1);
        assert_eq!(s.memory_list(None).unwrap().len(), 2);

        s.memory_set("decision/auth", "now 60 min expiry", Some("cursor-1"), &[])
            .unwrap();
        assert_eq!(s.memory_get("decision/auth").unwrap().unwrap().value, "now 60 min expiry");
    }

    #[test]
    fn messages_are_structured_and_track_read_state() {
        let mut s = Store::open_in_memory().unwrap();
        let id = s
            .send_message(
                "agent-1",
                "agent-2",
                "need-interface",
                json!({"resource": "PaymentProvider", "methods": ["authorize", "capture"]}),
                None,
            )
            .unwrap();
        let inbox = s.inbox("agent-2", true).unwrap();
        assert_eq!(inbox.len(), 1);
        assert_eq!(inbox[0].body["methods"][0], "authorize");

        assert!(s.mark_read(id, "agent-2").unwrap());
        assert!(s.inbox("agent-2", true).unwrap().is_empty());
        assert_eq!(s.inbox("agent-2", false).unwrap().len(), 1);
        // Nobody else can mark it read.
        assert!(!s.mark_read(id, "agent-3").unwrap());
    }
}
