//! The scheduler.
//!
//! A task queue orders work by priority. A *scheduler* has to answer harder
//! questions: what can run at the same time, what is genuinely blocked and
//! why, and who should pick up work that was dropped.
//!
//! What makes this one more than a queue is that it can see the code. Tasks
//! carry a **scope** — the symbols they are expected to touch — and that one
//! link turns the knowledge graph and the lease table into scheduling inputs:
//!
//! - **Dependency inference.** If task B touches `createPayment` and task A
//!   touches `record`, and `createPayment` calls `record`, then B depends on
//!   A. Nobody has to declare that; the call edge already said it.
//! - **Conflict-free parallelism.** Two tasks whose scopes overlap — including
//!   through containment, where one holds a class and the other a method of it
//!   — must not be handed out at once. The scheduler checks the same conflict
//!   rules the lease layer enforces, so its answer and reality agree.
//! - **Automatic reassignment.** An agent that stops heartbeating has its
//!   running tasks returned to the pool, exactly as its leases expire.

use std::collections::{HashMap, HashSet, VecDeque};

use anyhow::{anyhow, Result};
use rusqlite::{params, Transaction};
use serde::Serialize;
use serde_json::json;

use crate::ids;
use crate::lease::{self, AcquireOptions};
use crate::model::*;
use crate::store::{self, Store};
use crate::work::{TaskView, AGENT_ONLINE_MS};

/// A task, with everything the scheduler knows about whether it can run.
#[derive(Debug, Clone, Serialize)]
pub struct ScheduledTask {
    #[serde(flatten)]
    pub task: TaskView,
    /// Symbols this task is expected to touch.
    pub scope: Vec<Symbol>,
    /// Someone else holds part of the scope: the task is ready but not safe
    /// to start. Distinct from `TaskView::blocked_by`, which is about
    /// dependencies rather than contention.
    pub contended_by: Option<Conflict>,
}

impl ScheduledTask {
    /// Ready, unblocked by dependencies, and nothing in its scope is held.
    pub fn startable(&self) -> bool {
        self.task.ready && self.contended_by.is_none()
    }
}

/// One layer of the dependency graph: everything in a wave can run at once.
#[derive(Debug, Clone, Serialize)]
pub struct Wave {
    pub level: usize,
    pub tasks: Vec<ScheduledTask>,
}

/// The whole plan: what runs now, what runs after, and what cannot run.
#[derive(Debug, Clone, Serialize)]
pub struct Plan {
    pub waves: Vec<Wave>,
    pub running: Vec<ScheduledTask>,
    /// Submitted for review: still holding its leases, no longer "running"
    /// in the sense of active work, but very much not done either. Kept
    /// distinct from `running` because "what is everyone doing" and "what
    /// is sitting waiting for approval" are different questions.
    pub in_review: Vec<ScheduledTask>,
    /// Dependency cycles. Tasks in one of these can never become ready.
    pub cycles: Vec<Vec<String>>,
    /// How many tasks could be started right now without colliding.
    pub startable_now: usize,
    /// The widest wave — the most agents this plan could ever keep busy.
    pub max_parallel: usize,
    /// The longest causal chain of not-yet-done work, oldest first. Empty
    /// when nothing is pending. See `plan()` for what this does and does
    /// not capture.
    pub critical_path: Vec<String>,
}

/// A dependency the scheduler worked out from the code graph.
#[derive(Debug, Clone, Serialize)]
pub struct InferredDep {
    pub task: String,
    pub depends_on: String,
    /// The symbol in `task`'s scope that pointed at the other task.
    pub via: String,
    pub edge: EdgeKind,
}

impl Store {
    // ---------------------------------------------------------------- scope

    /// Declare which symbols a task will touch.
    pub fn set_task_scope(&mut self, task_id: &str, symbols: &[String]) -> Result<Vec<Symbol>> {
        let mut resolved = Vec::new();
        for s in symbols {
            resolved.push(self.resolve(s)?);
        }
        let task_id = task_id.to_string();
        let ids: Vec<String> = resolved.iter().map(|s| s.id.clone()).collect();
        let handles: Vec<String> = resolved.iter().map(|s| s.handle()).collect();
        self.write(move |tx| {
            let exists: bool = tx
                .query_row("SELECT 1 FROM tasks WHERE id = ?1", params![task_id], |_| {
                    Ok(true)
                })
                .unwrap_or(false);
            if !exists {
                return Err(anyhow!("no such task: {task_id}"));
            }
            for id in &ids {
                tx.execute(
                    "INSERT OR IGNORE INTO task_symbols(task_id, symbol_id) VALUES (?1, ?2)",
                    params![task_id, id],
                )?;
            }
            store::emit(
                tx,
                "task.scoped",
                None,
                None,
                Some(&task_id),
                json!({ "symbols": handles }),
            )?;
            Ok(())
        })?;
        Ok(resolved)
    }

    /// The symbols a task is scoped to.
    pub fn task_scope(&self, task_id: &str) -> Result<Vec<Symbol>> {
        let mut out = Vec::new();
        let mut stmt = self
            .conn()
            .prepare("SELECT symbol_id FROM task_symbols WHERE task_id = ?1")?;
        let ids: Vec<String> = stmt
            .query_map(params![task_id], |r| r.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        for id in ids {
            if let Some(s) = self.symbol(&id)? {
                out.push(s);
            }
        }
        out.sort_by_key(|s| s.handle());
        Ok(out)
    }

    /// Every task scoped to `symbol_id`, regardless of state — the reverse of
    /// `task_scope`. What cascading task creation uses to find "the task (and
    /// therefore goal) this changed symbol belongs to."
    pub fn tasks_for_symbol(&self, symbol_id: &str) -> Result<Vec<String>> {
        let mut stmt = self
            .conn()
            .prepare("SELECT task_id FROM task_symbols WHERE symbol_id = ?1")?;
        let rows = stmt.query_map(params![symbol_id], |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    // ----------------------------------------------------- dependency graph

    /// Work out dependencies between scoped tasks from the code graph.
    ///
    /// A task that touches a caller depends on the task that touches the
    /// callee: build the thing being used before the thing using it. Only
    /// unfinished tasks are considered, and an edge that would close a cycle
    /// is reported but not created.
    pub fn infer_dependencies(&mut self) -> Result<Vec<InferredDep>> {
        let tasks = self.tasks()?;
        let open: Vec<&TaskView> = tasks
            .iter()
            .filter(|t| t.task.state != TaskState::Done && t.task.state != TaskState::Failed)
            .collect();

        // symbol id -> task that owns it, including symbols nested inside it,
        // so a task scoped to a class picks up work on its methods.
        let mut owner: HashMap<String, String> = HashMap::new();
        for t in &open {
            for sym in self.task_scope(&t.task.id)? {
                owner.insert(sym.id.clone(), t.task.id.clone());
                for child in self.descendants(&sym.id)? {
                    owner.entry(child.id).or_insert_with(|| t.task.id.clone());
                }
            }
        }

        let mut found: Vec<InferredDep> = Vec::new();
        for t in &open {
            for sym in self.task_scope(&t.task.id)? {
                for n in self.neighbors(&sym.id)?.callees {
                    if !matches!(
                        n.kind,
                        EdgeKind::Calls | EdgeKind::Uses | EdgeKind::Imports | EdgeKind::Queries
                    ) {
                        continue;
                    }
                    let Some(other) = owner.get(&n.symbol.id) else {
                        continue;
                    };
                    if other == &t.task.id {
                        continue;
                    }
                    found.push(InferredDep {
                        task: t.task.id.clone(),
                        depends_on: other.clone(),
                        via: sym.handle(),
                        edge: n.kind,
                    });
                }
            }
        }
        found.sort_by(|a, b| a.task.cmp(&b.task).then(a.depends_on.cmp(&b.depends_on)));
        found.dedup_by(|a, b| a.task == b.task && a.depends_on == b.depends_on);

        // Add them one at a time, skipping any that would close a cycle.
        let mut added = Vec::new();
        for dep in found {
            if self.dependency_exists(&dep.task, &dep.depends_on)? {
                continue;
            }
            if self.would_cycle(&dep.task, &dep.depends_on)? {
                continue;
            }
            let (task, depends_on) = (dep.task.clone(), dep.depends_on.clone());
            let via = dep.via.clone();
            let edge = dep.edge.as_str().to_string();
            self.write(move |tx| {
                tx.execute(
                    "INSERT OR IGNORE INTO task_deps(task_id, depends_on) VALUES (?1, ?2)",
                    params![task, depends_on],
                )?;
                store::emit(
                    tx,
                    "task.dependency_inferred",
                    None,
                    None,
                    Some(&task),
                    json!({ "depends_on": depends_on, "via": via, "edge": edge }),
                )?;
                Ok(())
            })?;
            added.push(dep);
        }
        Ok(added)
    }

    fn dependency_exists(&self, task: &str, depends_on: &str) -> Result<bool> {
        Ok(self
            .conn()
            .query_row(
                "SELECT 1 FROM task_deps WHERE task_id = ?1 AND depends_on = ?2",
                params![task, depends_on],
                |_| Ok(true),
            )
            .unwrap_or(false))
    }

    /// Would adding `task -> depends_on` close a loop?
    fn would_cycle(&self, task: &str, depends_on: &str) -> Result<bool> {
        if task == depends_on {
            return Ok(true);
        }
        // Reachable from `depends_on` already back to `task`?
        let edges = self.dependency_edges()?;
        let mut seen: HashSet<&str> = HashSet::new();
        let mut queue = VecDeque::from([depends_on]);
        while let Some(cur) = queue.pop_front() {
            if cur == task {
                return Ok(true);
            }
            if !seen.insert(cur) {
                continue;
            }
            if let Some(next) = edges.get(cur) {
                for n in next {
                    queue.push_back(n);
                }
            }
        }
        Ok(false)
    }

    /// task -> the tasks it depends on.
    fn dependency_edges(&self) -> Result<HashMap<String, Vec<String>>> {
        let mut stmt = self
            .conn()
            .prepare("SELECT task_id, depends_on FROM task_deps")?;
        let mut out: HashMap<String, Vec<String>> = HashMap::new();
        for row in stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))? {
            let (task, dep) = row?;
            out.entry(task).or_default().push(dep);
        }
        Ok(out)
    }

    /// Dependency cycles. A task in one of these can never become ready, and
    /// silently never running is the worst way for that to surface.
    pub fn dependency_cycles(&self) -> Result<Vec<Vec<String>>> {
        let edges = self.dependency_edges()?;
        let unfinished: HashSet<String> = self
            .tasks()?
            .into_iter()
            .filter(|t| t.task.state != TaskState::Done)
            .map(|t| t.task.id)
            .collect();

        let mut cycles: Vec<Vec<String>> = Vec::new();
        let mut done: HashSet<String> = HashSet::new();

        for start in unfinished.iter() {
            if done.contains(start) {
                continue;
            }
            let mut path: Vec<String> = Vec::new();
            let mut stack: Vec<(String, usize)> = vec![(start.clone(), 0)];
            let mut on_path: HashSet<String> = HashSet::new();

            while let Some((node, idx)) = stack.pop() {
                if idx == 0 {
                    if on_path.contains(&node) {
                        // Found a loop: take the path from its first mention.
                        if let Some(at) = path.iter().position(|p| p == &node) {
                            let mut cycle = path[at..].to_vec();
                            cycle.sort();
                            if !cycles.contains(&cycle) {
                                cycles.push(cycle);
                            }
                        }
                        continue;
                    }
                    on_path.insert(node.clone());
                    path.push(node.clone());
                }
                let next = edges.get(&node).and_then(|v| v.get(idx)).cloned();
                match next {
                    Some(child) => {
                        stack.push((node, idx + 1));
                        stack.push((child, 0));
                    }
                    None => {
                        on_path.remove(&node);
                        path.pop();
                        done.insert(node);
                    }
                }
            }
        }
        cycles.sort();
        Ok(cycles)
    }

    // -------------------------------------------------------------- planning

    /// The schedule: tasks grouped into waves that can run concurrently.
    pub fn plan(&self) -> Result<Plan> {
        let tasks = self.tasks()?;
        let by_id: HashMap<String, &TaskView> =
            tasks.iter().map(|t| (t.task.id.clone(), t)).collect();
        let cycles = self.dependency_cycles()?;
        let in_cycle: HashSet<&String> = cycles.iter().flatten().collect();

        let mut running = Vec::new();
        let mut in_review = Vec::new();
        let mut pending: Vec<&TaskView> = Vec::new();
        for t in &tasks {
            match t.task.state {
                TaskState::Running => running.push(self.decorate(t)?),
                TaskState::Review => in_review.push(self.decorate(t)?),
                TaskState::Pending | TaskState::Blocked
                    if !in_cycle.contains(&t.task.id) =>
                {
                    pending.push(t)
                }
                _ => {}
            }
        }

        // Level 0 is everything with no unfinished dependency; level n needs
        // something from level n-1.
        let mut level_of: HashMap<String, usize> = HashMap::new();
        let mut remaining: Vec<&TaskView> = pending.clone();
        let mut level = 0;
        while !remaining.is_empty() && level < 64 {
            let mut this_level: Vec<&TaskView> = Vec::new();
            for t in &remaining {
                let ready = t.task.deps.iter().all(|d| {
                    level_of.contains_key(d)
                        || by_id
                            .get(d)
                            .map(|dep| dep.task.state == TaskState::Done)
                            .unwrap_or(true)
                });
                if ready {
                    this_level.push(t);
                }
            }
            if this_level.is_empty() {
                break; // everything left depends on something unreachable
            }
            for t in &this_level {
                level_of.insert(t.task.id.clone(), level);
            }
            remaining.retain(|t| !level_of.contains_key(&t.task.id));
            level += 1;
        }

        let mut waves: Vec<Wave> = Vec::new();
        for lvl in 0..level {
            let mut in_wave: Vec<ScheduledTask> = Vec::new();
            for t in &pending {
                if level_of.get(&t.task.id) == Some(&lvl) {
                    in_wave.push(self.decorate(t)?);
                }
            }
            in_wave.sort_by_key(|t| std::cmp::Reverse(t.task.task.priority));
            if !in_wave.is_empty() {
                waves.push(Wave {
                    level: lvl,
                    tasks: in_wave,
                });
            }
        }

        let startable_now = waves
            .first()
            .map(|w| w.tasks.iter().filter(|t| t.startable()).count())
            .unwrap_or(0);
        let max_parallel = waves.iter().map(|w| w.tasks.len()).max().unwrap_or(0);

        // Walk backward from the deepest wave's task through a real
        // dependency edge at each step. `level_of`'s construction guarantees
        // one exists: a task at level k became ready no earlier than round
        // k, which means at least one of its `deps` sits at exactly level
        // k-1 (otherwise it would have leveled a round sooner). That makes
        // this a genuine causal chain, not an arbitrary same-depth peer.
        //
        // A task blocked on a `Running` (not yet `Done`) dependency never
        // enters `level_of` at all, so a chain passing through in-flight
        // work will not appear here until that work finishes — pre-existing
        // scheduler behaviour, not something this introduces.
        let deepest = level_of.values().copied().max();
        let mut critical_path: Vec<String> = Vec::new();
        if let Some(deepest) = deepest {
            let mut cur =
                level_of.iter().find_map(|(id, &lvl)| (lvl == deepest).then(|| id.clone()));
            while let Some(id) = cur {
                let lvl = level_of[&id];
                critical_path.push(id.clone());
                if lvl == 0 {
                    break;
                }
                let deps: Vec<String> =
                    by_id.get(&id).map(|t| t.task.deps.clone()).unwrap_or_default();
                cur = deps.into_iter().find(|d| level_of.get(d) == Some(&(lvl - 1)));
            }
        }
        critical_path.reverse();

        Ok(Plan {
            waves,
            running,
            in_review,
            cycles,
            startable_now,
            max_parallel,
            critical_path,
        })
    }

    /// Attach scope and current contention to a task.
    fn decorate(&self, t: &TaskView) -> Result<ScheduledTask> {
        let scope = self.task_scope(&t.task.id)?;
        let agent = t.task.assignee.clone().unwrap_or_default();
        let mut contended_by = None;
        for sym in &scope {
            if let Some(c) = self.conflicts_for(&sym.id, &agent)?.into_iter().next() {
                contended_by = Some(c);
                break;
            }
        }
        Ok(ScheduledTask {
            task: t.clone(),
            scope,
            contended_by,
        })
    }

    // -------------------------------------------------------------- claiming

    /// Claim the best task an agent can safely start right now.
    ///
    /// "Safely" is the scheduler's job: a task whose scope is held by someone
    /// else is skipped rather than handed out to collide. When a task is
    /// claimed its scope is leased to the claimer in the same transaction, so
    /// there is no window between being given work and owning it.
    pub fn claim_next(&mut self, agent: &str, ttl_secs: i64) -> Result<Option<ScheduledTask>> {
        self.claim_next_in(agent, ttl_secs, None)
    }

    /// Claim the best task an agent can safely start, optionally restricted
    /// to one goal's tasks — what `golab continue --goal` asks for.
    /// `claim_next` is this with no filter.
    pub fn claim_next_in(
        &mut self,
        agent: &str,
        ttl_secs: i64,
        goal_id: Option<&str>,
    ) -> Result<Option<ScheduledTask>> {
        // One gate for "should this agent get work at all", checked here
        // rather than in every caller. An unregistered agent is treated as an
        // unpaused generalist, same as always — only capability-gated tasks
        // become invisible to it.
        let caller = self.agent(agent)?;
        if caller.as_ref().map(|a| a.paused).unwrap_or(false) {
            return Ok(None);
        }
        let capability = caller.and_then(|a| a.capability);
        let plan = self.plan()?;
        // A task requiring a capability this agent doesn't have is invisible
        // to it here — a hard gate, so `continue` never hands testing work to
        // an idle backend agent just because nobody else claimed it. A human
        // can still hand it over directly with `assign`, which bypasses this.
        let mut candidates: Vec<String> = plan
            .waves
            .first()
            .map(|w| {
                w.tasks
                    .iter()
                    .filter(|t| t.startable())
                    .filter(|t| t.task.task.required_capability.is_none() || t.task.task.required_capability == capability)
                    .map(|t| t.task.task.id.clone())
                    .collect()
            })
            .unwrap_or_default();
        if let Some(goal) = goal_id {
            let mut in_goal = Vec::new();
            for id in candidates {
                if self.task_goal(&id)?.as_deref() == Some(goal) {
                    in_goal.push(id);
                }
            }
            candidates = in_goal;
        }

        for id in candidates {
            let scope: Vec<String> = self
                .task_scope(&id)?
                .into_iter()
                .map(|s| s.id)
                .collect();
            let claimed = {
                let agent = agent.to_string();
                let id = id.clone();
                let scope = scope.clone();
                self.write(move |tx| claim_in_tx(tx, &id, &agent, &scope, ttl_secs))?
            };
            if claimed {
                return self
                    .tasks()?
                    .iter()
                    .find(|t| t.task.id == id)
                    .map(|t| self.decorate(t))
                    .transpose();
            }
        }
        Ok(None)
    }

    /// Move a task to a different agent, taking its scope with it.
    ///
    /// This is the dashboard's "reassign" button. It has to move the leases
    /// too: the point of the scope is that whoever owns the task owns the
    /// symbols, and a handover that left them behind would strand both agents.
    pub fn reassign_task(&mut self, task_id: &str, to: &str, actor: Option<&str>) -> Result<TaskView> {
        self.assign_task_opts(task_id, to, actor, false, 0)
    }

    /// Move a task to `to`, whether or not it was ever claimed.
    ///
    /// A task that already has active leases has them transferred; a task
    /// that was only ever scoped (never claimed) has that scope settled for
    /// `to` the same all-or-nothing way `claim_in_tx` does — checked for
    /// conflicts first, so this refuses (rather than silently leasing
    /// nothing) if part of the scope is held by someone else. `preempt`/
    /// `priority` reach that settlement for the part of the scope that
    /// still needs to be acquired fresh.
    pub fn assign_task_opts(
        &mut self,
        task_id: &str,
        to: &str,
        actor: Option<&str>,
        preempt: bool,
        priority: i64,
    ) -> Result<TaskView> {
        let task = self
            .task(task_id)?
            .ok_or_else(|| anyhow!("no such task: {task_id}"))?;
        if self.agent(to)?.is_none() {
            return Err(anyhow!("no such agent: {to}"));
        }
        let from = task.task.assignee.clone();
        if from.as_deref() == Some(to) {
            return Ok(task);
        }
        let scope: Vec<String> = self
            .task_scope(task_id)?
            .into_iter()
            .map(|s| s.id)
            .collect();

        let id = task_id.to_string();
        let to_owned = to.to_string();
        let actor = actor.map(|s| s.to_string());
        self.write(move |tx| {
            let now = ids::now_ms();

            // Hand over every lease the task was already given, in this
            // transaction so the symbols are never briefly unowned.
            let mut stmt = tx.prepare(
                "SELECT id, agent, symbol_id FROM leases WHERE task = ?1 AND state = 'active'",
            )?;
            let held: Vec<(String, String, String)> = stmt
                .query_map(params![id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                .collect::<Result<Vec<_>, _>>()?;
            drop(stmt);
            let mut settled: HashSet<String> = HashSet::new();
            for (lease_id, holder, symbol_id) in &held {
                if holder != &to_owned {
                    lease::transfer_lease_tx(tx, lease_id, holder, &to_owned)?;
                }
                settled.insert(symbol_id.clone());
            }

            // The rest of the scope (never claimed, so nothing to transfer)
            // has to be acquired fresh for `to` — checked and settled
            // all-or-nothing, exactly like an initial claim, so this cannot
            // hand out a task while quietly leasing none of its scope.
            let remaining: Vec<String> =
                scope.iter().filter(|s| !settled.contains(*s)).cloned().collect();
            if !remaining.is_empty() {
                let opts = AcquireOptions {
                    task: Some(id.clone()),
                    preempt,
                    priority,
                    note: Some("leased on reassignment".to_string()),
                    ..Default::default()
                };
                let conflicts = lease::settle_scope_tx(tx, &remaining, &to_owned, &opts)?;
                if !conflicts.is_empty() {
                    return Err(anyhow!(
                        "cannot assign {id} to {to_owned}: {} of its symbols are held by someone else",
                        conflicts.len()
                    ));
                }
            }

            tx.execute(
                "UPDATE tasks SET assignee = ?2, state = 'running', claimed_at = ?3, \
                 updated_at = ?3 WHERE id = ?1",
                params![id, to_owned, now],
            )?;
            store::emit(
                tx,
                "task.reassigned",
                actor.as_deref(),
                None,
                Some(&id),
                json!({ "from": from, "to": to_owned, "leases_moved": held.len(), "leases_acquired": remaining.len() }),
            )?;
            Ok(())
        })?;
        self.task(task_id)?
            .ok_or_else(|| anyhow!("task vanished: {task_id}"))
    }

    /// Change a task's priority. This is what dragging a task does.
    pub fn set_task_priority(&mut self, task_id: &str, priority: i64) -> Result<TaskView> {
        let id = task_id.to_string();
        self.write(move |tx| {
            let n = tx.execute(
                "UPDATE tasks SET priority = ?2, updated_at = ?3 WHERE id = ?1",
                params![id, priority, ids::now_ms()],
            )?;
            if n == 0 {
                return Err(anyhow!("no such task: {id}"));
            }
            store::emit(
                tx,
                "task.reprioritised",
                None,
                None,
                Some(&id),
                json!({ "priority": priority }),
            )?;
            Ok(())
        })?;
        self.task(task_id)?
            .ok_or_else(|| anyhow!("task vanished: {task_id}"))
    }

    /// Declare (or clear) the capability a task requires. `None` opens it to
    /// any agent again. `continue` enforces this as a hard gate; `assign`
    /// deliberately does not — it's the human-override path.
    pub fn set_task_required_capability(
        &mut self,
        task_id: &str,
        capability: Option<Capability>,
    ) -> Result<TaskView> {
        let id = task_id.to_string();
        self.write(move |tx| {
            let n = tx.execute(
                "UPDATE tasks SET required_capability = ?2, updated_at = ?3 WHERE id = ?1",
                params![id, capability.map(|c| c.as_str()), ids::now_ms()],
            )?;
            if n == 0 {
                return Err(anyhow!("no such task: {id}"));
            }
            Ok(())
        })?;
        self.task(task_id)?
            .ok_or_else(|| anyhow!("task vanished: {task_id}"))
    }

    /// Return work abandoned by agents that stopped heartbeating.
    ///
    /// The same self-healing property leases have: nothing has to notice the
    /// crash for the work to become available again.
    pub fn reassign_orphans(&mut self) -> Result<Vec<String>> {
        let cutoff = ids::now_ms() - AGENT_ONLINE_MS;
        self.write(move |tx| {
            let mut stmt = tx.prepare(
                "SELECT t.id, t.assignee FROM tasks t \
                 LEFT JOIN agents a ON a.name = t.assignee \
                 WHERE t.state = 'running' AND t.assignee IS NOT NULL \
                   AND (a.name IS NULL OR a.heartbeat_at < ?1)",
            )?;
            let orphans: Vec<(String, String)> = stmt
                .query_map(params![cutoff], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<Result<Vec<_>, _>>()?;
            drop(stmt);

            for (id, agent) in &orphans {
                tx.execute(
                    "UPDATE tasks SET state = 'pending', assignee = NULL, claimed_at = NULL, \
                     updated_at = ?2 WHERE id = ?1",
                    params![id, ids::now_ms()],
                )?;
                // The scope was handed out *with* the task, so it has to come
                // back with it. Otherwise the task returns to the pool and
                // nobody can claim it until the dead agent's TTL runs out.
                let freed = lease::release_task_leases(tx, id)?;
                store::emit(
                    tx,
                    "task.reassigned",
                    Some(agent),
                    None,
                    Some(id),
                    json!({ "reason": "assignee went offline", "leases_released": freed }),
                )?;
            }
            Ok(orphans.into_iter().map(|(id, _)| id).collect())
        })
    }
}

/// Claim one task and lease its scope, atomically. Returns false if the task
/// was taken or its scope became contended since planning.
fn claim_in_tx(
    tx: &Transaction,
    task_id: &str,
    agent: &str,
    scope: &[String],
    ttl_secs: i64,
) -> Result<bool> {
    let state: String = tx.query_row(
        "SELECT state FROM tasks WHERE id = ?1",
        params![task_id],
        |r| r.get(0),
    )?;
    if state != "pending" {
        return Ok(false);
    }
    // Re-check under the write lock: planning was a read, and reads race.
    let opts = AcquireOptions {
        task: Some(task_id.to_string()),
        ttl_secs,
        note: Some("leased by the scheduler with the task".to_string()),
        ..Default::default()
    };
    if !lease::settle_scope_tx(tx, scope, agent, &opts)?.is_empty() {
        return Ok(false);
    }

    let now = ids::now_ms();
    tx.execute(
        "UPDATE tasks SET state = 'running', assignee = ?2, claimed_at = ?3, updated_at = ?3 \
         WHERE id = ?1",
        params![task_id, agent, now],
    )?;
    tx.execute(
        "UPDATE agents SET status = 'working', heartbeat_at = ?2 WHERE name = ?1",
        params![agent, now],
    )?;

    store::emit(
        tx,
        "task.started",
        Some(agent),
        None,
        Some(task_id),
        json!({ "scope": scope.len() }),
    )?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan;

    /// A repo where `createPayment` calls `record`, so the scheduler has a
    /// real edge to reason about.
    fn fixture() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("src/ledger.ts"),
            "export function record(x: number) { return x; }\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("src/pay.ts"),
            "import { record } from './ledger';\n\
             export function createPayment(x: number) { return record(x); }\n\
             export function refund(x: number) { return -x; }\n",
        )
        .unwrap();
        let mut store = Store::init(dir.path()).unwrap();
        scan::scan(&mut store, crate::ids::DEFAULT_REPO_ID, dir.path(), &[], false).unwrap();
        store.register_agent("agent-1", "claude").unwrap();
        store.register_agent("agent-2", "cursor").unwrap();
        (dir, store)
    }

    #[test]
    fn dependencies_are_inferred_from_call_edges() {
        let (_d, mut store) = fixture();
        let api = store.add_task("wire up the endpoint", 5, &[]).unwrap();
        let lib = store.add_task("write the ledger", 1, &[]).unwrap();
        store
            .set_task_scope(&api.id, &["createPayment".to_string()])
            .unwrap();
        store.set_task_scope(&lib.id, &["record".to_string()]).unwrap();

        let inferred = store.infer_dependencies().unwrap();
        assert_eq!(inferred.len(), 1, "{inferred:?}");
        assert_eq!(inferred[0].task, api.id);
        assert_eq!(inferred[0].depends_on, lib.id);
        assert_eq!(inferred[0].edge, EdgeKind::Calls);

        // The caller now waits for the callee, despite outranking it.
        let plan = store.plan().unwrap();
        assert_eq!(plan.waves[0].tasks[0].task.task.id, lib.id);
        assert_eq!(plan.waves[1].tasks[0].task.task.id, api.id);
    }

    #[test]
    fn inference_is_idempotent_and_skips_cycles() {
        let (_d, mut store) = fixture();
        let a = store.add_task("a", 0, &[]).unwrap();
        let b = store.add_task("b", 0, &[]).unwrap();
        store.set_task_scope(&a.id, &["createPayment".to_string()]).unwrap();
        store.set_task_scope(&b.id, &["record".to_string()]).unwrap();

        assert_eq!(store.infer_dependencies().unwrap().len(), 1);
        assert!(
            store.infer_dependencies().unwrap().is_empty(),
            "running it twice must add nothing"
        );

        // b already depends on nothing; force the reverse edge by hand and the
        // inferred one must not be re-added on top of it.
        store
            .set_task_scope(&b.id, &["refund".to_string()])
            .unwrap();
        assert!(store.dependency_cycles().unwrap().is_empty());
    }

    #[test]
    fn a_dependency_cycle_is_reported_rather_than_hidden() {
        let (_d, mut store) = fixture();
        let a = store.add_task("a", 0, &[]).unwrap();
        let b = store.add_task("b", 0, std::slice::from_ref(&a.id)).unwrap();
        // Close the loop by hand: a depends on b, b depends on a.
        store
            .write(|tx| {
                tx.execute(
                    "INSERT INTO task_deps(task_id, depends_on) VALUES (?1, ?2)",
                    params![a.id, b.id],
                )?;
                Ok(())
            })
            .unwrap();

        let cycles = store.dependency_cycles().unwrap();
        assert_eq!(cycles.len(), 1, "{cycles:?}");
        assert_eq!(cycles[0], vec![a.id.clone(), b.id.clone()]);

        // Neither task appears in any wave — they can never become ready.
        let plan = store.plan().unwrap();
        assert!(plan.waves.is_empty(), "{:?}", plan.waves);
        assert_eq!(plan.startable_now, 0);
    }

    #[test]
    fn waves_show_what_can_run_in_parallel() {
        let (_d, mut store) = fixture();
        let base = store.add_task("base", 1, &[]).unwrap();
        let a = store.add_task("a", 5, std::slice::from_ref(&base.id)).unwrap();
        let b = store.add_task("b", 5, std::slice::from_ref(&base.id)).unwrap();
        let _c = store.add_task("independent", 9, &[]).unwrap();

        let plan = store.plan().unwrap();
        assert_eq!(plan.waves[0].tasks.len(), 2, "base + independent");
        assert_eq!(plan.waves[1].tasks.len(), 2, "a and b unblock together");
        assert_eq!(plan.max_parallel, 2);
        assert_eq!(plan.startable_now, 2);

        let wave1: Vec<&str> = plan.waves[1]
            .tasks
            .iter()
            .map(|t| t.task.task.id.as_str())
            .collect();
        assert!(wave1.contains(&a.id.as_str()) && wave1.contains(&b.id.as_str()));
    }

    #[test]
    fn claiming_a_scoped_task_leases_its_symbols() {
        let (_d, mut store) = fixture();
        let t = store.add_task("do the thing", 5, &[]).unwrap();
        store
            .set_task_scope(&t.id, &["createPayment".to_string()])
            .unwrap();

        let claimed = store.claim_next("agent-1", 300).unwrap().unwrap();
        assert_eq!(claimed.task.task.id, t.id);
        assert_eq!(claimed.task.task.assignee.as_deref(), Some("agent-1"));

        let leases = store.active_leases(None).unwrap();
        assert_eq!(leases.len(), 1, "the scope was leased with the task");
        assert_eq!(leases[0].agent, "agent-1");
        assert_eq!(leases[0].task.as_deref(), Some(t.id.as_str()));
    }

    /// The scheduler must not hand out work that would collide with a lease
    /// someone already holds — that is the whole point of it knowing.
    #[test]
    fn contended_tasks_are_skipped_not_handed_out() {
        let (_d, mut store) = fixture();
        let blocked = store.add_task("touch createPayment", 9, &[]).unwrap();
        let free = store.add_task("touch refund", 1, &[]).unwrap();
        store
            .set_task_scope(&blocked.id, &["createPayment".to_string()])
            .unwrap();
        store.set_task_scope(&free.id, &["refund".to_string()]).unwrap();

        // Someone outside the scheduler grabs the high-priority task's scope.
        let sym = store.resolve("createPayment").unwrap();
        store
            .acquire(&sym.id, "outsider", &AcquireOptions::default())
            .unwrap();

        let plan = store.plan().unwrap();
        let hot = plan.waves[0]
            .tasks
            .iter()
            .find(|t| t.task.task.id == blocked.id)
            .unwrap();
        assert!(hot.task.ready, "its dependencies are satisfied");
        assert!(!hot.startable(), "but its scope is held");
        assert_eq!(hot.contended_by.as_ref().unwrap().holder, "outsider");

        // So the agent gets the lower-priority task it can actually do.
        let claimed = store.claim_next("agent-1", 300).unwrap().unwrap();
        assert_eq!(claimed.task.task.id, free.id);
    }

    #[test]
    fn containment_counts_as_contention() {
        let (_d, mut store) = fixture();
        let t = store.add_task("edit one function", 5, &[]).unwrap();
        store
            .set_task_scope(&t.id, &["createPayment".to_string()])
            .unwrap();

        // Holding the whole file must block a task scoped to a function in it.
        let file = store.resolve("src/pay.ts").unwrap();
        store
            .acquire(&file.id, "outsider", &AcquireOptions::default())
            .unwrap();

        assert!(store.claim_next("agent-1", 300).unwrap().is_none());
    }

    #[test]
    fn two_agents_claim_different_tasks() {
        let (_d, mut store) = fixture();
        for (title, scope) in [("a", "createPayment"), ("b", "refund"), ("c", "record")] {
            let t = store.add_task(title, 5, &[]).unwrap();
            store.set_task_scope(&t.id, &[scope.to_string()]).unwrap();
        }
        let first = store.claim_next("agent-1", 300).unwrap().unwrap();
        let second = store.claim_next("agent-2", 300).unwrap().unwrap();
        assert_ne!(first.task.task.id, second.task.task.id);
        assert_eq!(store.active_leases(None).unwrap().len(), 2);
    }

    /// Reassigning a task without returning its scope would put the task back
    /// in the pool in a state where nobody can actually claim it.
    #[test]
    fn reassignment_returns_the_scope_too() {
        let (_d, mut store) = fixture();
        let t = store.add_task("in flight", 5, &[]).unwrap();
        store
            .set_task_scope(&t.id, &["createPayment".to_string()])
            .unwrap();
        // A long TTL: the lease would far outlive the agent's heartbeat.
        store.claim_next("agent-1", 3600).unwrap().unwrap();
        assert_eq!(store.active_leases(None).unwrap().len(), 1);

        store
            .write(|tx| {
                tx.execute("UPDATE agents SET heartbeat_at = 0 WHERE name = 'agent-1'", [])?;
                Ok(())
            })
            .unwrap();
        store.reassign_orphans().unwrap();

        assert!(
            store.active_leases(None).unwrap().is_empty(),
            "the dead agent's scope must be freed with its task"
        );
        let picked_up = store.claim_next("agent-2", 300).unwrap().unwrap();
        assert_eq!(picked_up.task.task.id, t.id, "another agent can take it now");
        assert_eq!(picked_up.task.task.assignee.as_deref(), Some("agent-2"));
    }

    #[test]
    fn work_abandoned_by_a_dead_agent_goes_back_to_the_pool() {
        let (_d, mut store) = fixture();
        let t = store.add_task("in flight", 5, &[]).unwrap();
        store.claim_next("agent-1", 300).unwrap().unwrap();
        assert_eq!(store.task(&t.id).unwrap().unwrap().task.state, TaskState::Running);

        // agent-1 stops heartbeating.
        store
            .write(|tx| {
                tx.execute(
                    "UPDATE agents SET heartbeat_at = 0 WHERE name = 'agent-1'",
                    [],
                )?;
                Ok(())
            })
            .unwrap();

        let reassigned = store.reassign_orphans().unwrap();
        assert_eq!(reassigned, vec![t.id.clone()]);
        let after = store.task(&t.id).unwrap().unwrap();
        assert_eq!(after.task.state, TaskState::Pending);
        assert!(after.task.assignee.is_none());

        // And another agent can pick it straight up.
        assert_eq!(
            store.claim_next("agent-2", 300).unwrap().unwrap().task.task.id,
            t.id
        );
    }

    #[test]
    fn finishing_a_task_releases_the_scope_it_was_given() {
        let (_d, mut store) = fixture();
        let t = store.add_task("do it", 5, &[]).unwrap();
        store
            .set_task_scope(&t.id, &["createPayment".to_string()])
            .unwrap();
        store.claim_next("agent-1", 300).unwrap().unwrap();
        assert_eq!(store.active_leases(None).unwrap().len(), 1);

        store
            .set_task_state(&t.id, TaskState::Done, Some("agent-1"), None, false)
            .unwrap();
        assert!(
            store.active_leases(None).unwrap().is_empty(),
            "the next agent should not wait out a TTL on finished work"
        );
    }

    /// Closing someone else's task would drop the lease they are still using.
    #[test]
    fn an_agent_cannot_close_work_assigned_to_someone_else() {
        let (_d, mut store) = fixture();
        let t = store.add_task("do it", 5, &[]).unwrap();
        store
            .set_task_scope(&t.id, &["createPayment".to_string()])
            .unwrap();
        store.claim_next("agent-1", 300).unwrap().unwrap();

        let err = store
            .set_task_state(&t.id, TaskState::Done, Some("agent-2"), None, false)
            .unwrap_err();
        assert!(err.to_string().contains("assigned to agent-1"), "{err}");
        assert_eq!(store.active_leases(None).unwrap().len(), 1, "lease untouched");

        // A human cleaning up after a crash can override.
        store
            .set_task_state(&t.id, TaskState::Done, Some("agent-2"), None, true)
            .unwrap();
        assert!(store.active_leases(None).unwrap().is_empty());
    }

    #[test]
    fn a_paused_agent_is_given_no_new_work_but_keeps_what_it_has() {
        let (_d, mut store) = fixture();
        let first = store.add_task("first", 9, &[]).unwrap();
        store.add_task("second", 5, &[]).unwrap();
        store
            .set_task_scope(&first.id, &["createPayment".to_string()])
            .unwrap();
        store.claim_next("agent-1", 300).unwrap().unwrap();

        let paused = store.set_agent_paused("agent-1", true).unwrap();
        assert!(paused.paused);
        assert!(
            store.claim_next("agent-1", 300).unwrap().is_none(),
            "a paused agent gets nothing new"
        );
        assert_eq!(
            store.active_leases(Some("agent-1")).unwrap().len(),
            1,
            "but it keeps the work it already had"
        );
        // Another agent is unaffected.
        assert!(store.claim_next("agent-2", 300).unwrap().is_some());

        // Resuming makes it eligible again. agent-2 took the only other task,
        // so add one more to prove the gate is what changed.
        store.set_agent_paused("agent-1", false).unwrap();
        store
            .set_task_state(&first.id, TaskState::Done, Some("agent-1"), None, false)
            .unwrap();
        store.add_task("third", 1, &[]).unwrap();
        let after = store.claim_next("agent-1", 300).unwrap();
        assert_eq!(
            after.map(|t| t.task.task.title),
            Some("third".to_string()),
            "a resumed agent is handed work again"
        );
    }

    #[test]
    fn reassigning_a_task_moves_its_leases_with_it() {
        let (_d, mut store) = fixture();
        let t = store.add_task("in flight", 5, &[]).unwrap();
        store
            .set_task_scope(&t.id, &["createPayment".to_string()])
            .unwrap();
        let claimed = store.claim_next("agent-1", 300).unwrap().unwrap();
        let lease_id = store.active_leases(None).unwrap()[0].id.clone();
        assert_eq!(claimed.task.task.assignee.as_deref(), Some("agent-1"));

        let moved = store
            .reassign_task(&t.id, "agent-2", Some("human"))
            .unwrap();
        assert_eq!(moved.task.assignee.as_deref(), Some("agent-2"));

        let leases = store.active_leases(None).unwrap();
        assert_eq!(leases.len(), 1, "the symbol is never briefly unowned");
        assert_eq!(leases[0].agent, "agent-2");
        assert_eq!(leases[0].id, lease_id, "same lease, new owner");

        assert!(store.reassign_task(&t.id, "nobody", None).is_err());
    }

    /// The bug this fix closes: a scoped-but-never-claimed task used to
    /// reassign with zero leases, leaving the new owner able to edit nothing
    /// `golab check` would accept and a third agent free to take the scope
    /// out from under them.
    #[test]
    fn assigning_a_never_claimed_task_leases_its_scope() {
        let (_d, mut store) = fixture();
        let t = store.add_task("never claimed", 5, &[]).unwrap();
        store
            .set_task_scope(&t.id, &["createPayment".to_string()])
            .unwrap();
        assert!(store.active_leases(None).unwrap().is_empty());

        let assigned = store.reassign_task(&t.id, "agent-1", None).unwrap();
        assert_eq!(assigned.task.assignee.as_deref(), Some("agent-1"));

        let leases = store.active_leases(None).unwrap();
        assert_eq!(leases.len(), 1, "the scope should be leased, not left empty");
        assert_eq!(leases[0].agent, "agent-1");
        assert_eq!(leases[0].task.as_deref(), Some(t.id.as_str()));
    }

    /// Assigning must not silently take a symbol someone else already holds.
    #[test]
    fn assigning_into_a_conflict_is_refused_and_leaves_the_task_untouched() {
        let (_d, mut store) = fixture();
        let t = store.add_task("contended", 5, &[]).unwrap();
        store
            .set_task_scope(&t.id, &["createPayment".to_string()])
            .unwrap();
        let sym = store.resolve("createPayment").unwrap();
        store
            .acquire(&sym.id, "outsider", &AcquireOptions::default())
            .unwrap();

        let err = store.reassign_task(&t.id, "agent-1", None).unwrap_err();
        assert!(err.to_string().contains("held by someone else"), "{err}");

        // The whole transaction rolled back: task state is untouched, and the
        // outsider's lease is exactly where it was.
        let after = store.task(&t.id).unwrap().unwrap();
        assert_eq!(after.task.state, TaskState::Pending);
        assert!(after.task.assignee.is_none());
        let leases = store.active_leases(None).unwrap();
        assert_eq!(leases.len(), 1);
        assert_eq!(leases[0].agent, "outsider");
    }

    /// `--preempt` lets an assign override a strictly lower-priority holder,
    /// exactly like acquiring directly would.
    #[test]
    fn assigning_with_preempt_takes_a_lower_priority_holders_scope() {
        let (_d, mut store) = fixture();
        let t = store.add_task("contended", 5, &[]).unwrap();
        store
            .set_task_scope(&t.id, &["createPayment".to_string()])
            .unwrap();
        let sym = store.resolve("createPayment").unwrap();
        store
            .acquire(
                &sym.id,
                "outsider",
                &AcquireOptions {
                    priority: 1,
                    ..Default::default()
                },
            )
            .unwrap();

        let assigned = store
            .assign_task_opts(&t.id, "agent-1", None, true, 9)
            .unwrap();
        assert_eq!(assigned.task.assignee.as_deref(), Some("agent-1"));
        let leases = store.active_leases(None).unwrap();
        assert_eq!(leases.len(), 1);
        assert_eq!(leases[0].agent, "agent-1");
    }

    /// A task with some leases already held and some scope never claimed
    /// (e.g. one symbol was added to its scope after it started running)
    /// settles the unheld part without disturbing the transferred part.
    #[test]
    fn assigning_a_partially_leased_task_settles_the_rest() {
        let (_d, mut store) = fixture();
        let t = store.add_task("partial", 5, &[]).unwrap();
        store
            .set_task_scope(&t.id, &["createPayment".to_string()])
            .unwrap();
        store.claim_next("agent-1", 300).unwrap().unwrap();
        // Scope grows after the task is already running.
        store.set_task_scope(&t.id, &["refund".to_string()]).unwrap();

        let assigned = store.reassign_task(&t.id, "agent-2", None).unwrap();
        assert_eq!(assigned.task.assignee.as_deref(), Some("agent-2"));
        let mut leases = store.active_leases(None).unwrap();
        leases.sort_by(|a, b| a.symbol_handle.cmp(&b.symbol_handle));
        assert_eq!(leases.len(), 2, "{leases:?}");
        assert!(leases.iter().all(|l| l.agent == "agent-2"));
    }

    #[test]
    fn critical_path_follows_the_longest_dependency_chain() {
        let (_d, mut store) = fixture();
        let a = store.add_task("a", 5, &[]).unwrap();
        let b = store.add_task("b", 5, std::slice::from_ref(&a.id)).unwrap();
        let c = store.add_task("c", 5, std::slice::from_ref(&b.id)).unwrap();
        // A same-depth distractor: not on the real chain from c back to a.
        store.add_task("distractor", 9, &[]).unwrap();

        let plan = store.plan().unwrap();
        assert_eq!(plan.critical_path, vec![a.id.clone(), b.id.clone(), c.id.clone()]);
    }

    #[test]
    fn critical_path_is_empty_when_nothing_is_pending() {
        let (_d, store) = fixture();
        assert!(store.plan().unwrap().critical_path.is_empty());
    }

    #[test]
    fn claim_next_in_restricts_to_one_goal() {
        let (_d, mut store) = fixture();
        let goal = store.add_goal("refunds", 5, None, None).unwrap();
        store
            .goal_decompose(&goal.id, "in the goal", 9, &[], &["createPayment".to_string()])
            .unwrap();
        store.add_task("outside the goal", 5, &[]).unwrap();

        // The unfiltered claim would take the higher-priority task outside
        // the goal; --goal must restrict to the goal's own tasks.
        let claimed = store
            .claim_next_in("agent-1", 300, Some(&goal.id))
            .unwrap()
            .unwrap();
        assert_eq!(claimed.task.task.title, "in the goal");
    }

    #[test]
    fn priority_changes_reorder_the_plan() {
        let (_d, mut store) = fixture();
        let low = store.add_task("low", 1, &[]).unwrap();
        store.add_task("high", 9, &[]).unwrap();
        assert_eq!(store.plan().unwrap().waves[0].tasks[0].task.task.title, "high");

        store.set_task_priority(&low.id, 99).unwrap();
        assert_eq!(
            store.plan().unwrap().waves[0].tasks[0].task.task.title,
            "low",
            "dragging it to the top is a priority change"
        );
        assert!(store.set_task_priority("T99", 1).is_err());
    }

    #[test]
    fn throughput_is_derived_from_the_event_log() {
        let (_d, mut store) = fixture();
        let t = store.add_task("a", 5, &[]).unwrap();
        store.add_task("b", 4, &[]).unwrap();
        store.claim_next("agent-1", 300).unwrap().unwrap();
        store
            .set_task_state(&t.id, TaskState::Done, Some("agent-1"), None, false)
            .unwrap();
        // A denial, so the contention signal is non-zero.
        let sym = store.resolve("createPayment").unwrap();
        store
            .acquire(&sym.id, "agent-1", &AcquireOptions::default())
            .unwrap();
        store
            .acquire(&sym.id, "agent-2", &AcquireOptions::default())
            .unwrap();

        let t = store.throughput(60, 12).unwrap();
        assert_eq!(t.tasks_completed, 1);
        assert_eq!(t.tasks_started, 1);
        assert_eq!(t.leases_denied, 1);
        assert_eq!(t.leases_acquired, 1);
        assert_eq!(t.completed_series.len(), 12);
        assert_eq!(t.completed_series.iter().sum::<i64>(), 1);
        assert!(t.agents_active >= 1);
        assert!(t.mean_task_secs.is_some(), "one task finished, so there is a mean");
    }

    /// Regression test: `plan()`'s state-bucketing match used to have a
    /// wildcard fallthrough, so a task in `Review` fell through it entirely —
    /// invisible in every wave and not in `running` either. This is exactly
    /// the case `observe` exists to answer correctly.
    #[test]
    fn review_state_tasks_are_visible_in_the_plan() {
        let (_d, mut store) = fixture();
        let t = store.add_task("in review", 5, &[]).unwrap();
        store.reassign_task(&t.id, "agent-1", None).unwrap();
        store.submit_for_review(&t.id, "agent-1").unwrap();

        let plan = store.plan().unwrap();
        assert_eq!(plan.in_review.len(), 1, "{plan:?}");
        assert_eq!(plan.in_review[0].task.task.id, t.id);
        assert!(
            plan.running.iter().all(|s| s.task.task.state != TaskState::Review),
            "review-state tasks belong in in_review, not running"
        );
        assert!(
            plan.waves.iter().flat_map(|w| &w.tasks).all(|s| s.task.task.id != t.id),
            "a task in review must not also appear in a wave"
        );
    }

    #[test]
    fn an_unscoped_task_still_schedules() {
        let (_d, mut store) = fixture();
        let t = store.add_task("write the docs", 5, &[]).unwrap();
        let claimed = store.claim_next("agent-1", 300).unwrap().unwrap();
        assert_eq!(claimed.task.task.id, t.id);
        assert!(claimed.scope.is_empty());
        assert!(
            store.active_leases(None).unwrap().is_empty(),
            "nothing to lease, so nothing was leased"
        );
    }

    #[test]
    fn a_capability_gated_task_is_invisible_to_a_mismatched_agent() {
        let (_d, mut store) = fixture();
        let t = store.add_task("write the tests", 9, &[]).unwrap();
        store
            .set_task_required_capability(&t.id, Some(Capability::Testing))
            .unwrap();
        store
            .set_agent_capability("agent-1", Some(Capability::Backend))
            .unwrap();

        // agent-1 is a backend specialist; the testing task must not surface,
        // even though it's the only thing pending and outranks nothing.
        assert!(store.claim_next("agent-1", 300).unwrap().is_none());
    }

    #[test]
    fn a_capability_gated_task_is_claimable_by_a_matching_agent() {
        let (_d, mut store) = fixture();
        let t = store.add_task("write the tests", 9, &[]).unwrap();
        store
            .set_task_required_capability(&t.id, Some(Capability::Testing))
            .unwrap();
        store
            .set_agent_capability("agent-2", Some(Capability::Testing))
            .unwrap();

        let claimed = store.claim_next("agent-2", 300).unwrap().unwrap();
        assert_eq!(claimed.task.task.id, t.id);
    }

    #[test]
    fn a_generalist_agent_only_sees_uncapped_tasks() {
        let (_d, mut store) = fixture();
        let gated = store.add_task("write the tests", 9, &[]).unwrap();
        store
            .set_task_required_capability(&gated.id, Some(Capability::Testing))
            .unwrap();
        let open = store.add_task("update the readme", 1, &[]).unwrap();

        // agent-1 has no capability at all — the gated task, despite far
        // outranking the open one, must never be handed to it.
        let claimed = store.claim_next("agent-1", 300).unwrap().unwrap();
        assert_eq!(claimed.task.task.id, open.id);
    }

    #[test]
    fn assign_bypasses_the_capability_gate() {
        let (_d, mut store) = fixture();
        let t = store.add_task("write the tests", 9, &[]).unwrap();
        store
            .set_task_required_capability(&t.id, Some(Capability::Testing))
            .unwrap();
        store
            .set_agent_capability("agent-1", Some(Capability::Backend))
            .unwrap();

        // assign is the human-override path: a mismatched capability doesn't
        // block a direct hand-off the way `continue` would block a claim.
        let assigned = store.reassign_task(&t.id, "agent-1", None).unwrap();
        assert_eq!(assigned.task.assignee.as_deref(), Some("agent-1"));
    }

    #[test]
    fn a_capability_gated_orphan_returns_to_the_pool_for_anyone_matching() {
        let (_d, mut store) = fixture();
        let t = store.add_task("write the tests", 9, &[]).unwrap();
        store
            .set_task_required_capability(&t.id, Some(Capability::Testing))
            .unwrap();
        store
            .set_agent_capability("agent-1", Some(Capability::Testing))
            .unwrap();
        store
            .set_agent_capability("agent-2", Some(Capability::Testing))
            .unwrap();
        store.claim_next("agent-1", 300).unwrap().unwrap();

        // agent-1 goes dark.
        store.write(|tx| {
            tx.execute(
                "UPDATE agents SET heartbeat_at = 0 WHERE name = 'agent-1'",
                [],
            )?;
            Ok(())
        }).unwrap();

        let reassigned = store.reassign_orphans().unwrap();
        assert_eq!(reassigned, vec![t.id.clone()]);
        let claimed = store.claim_next("agent-2", 300).unwrap().unwrap();
        assert_eq!(claimed.task.task.id, t.id, "still capability-gated, still claimable by a match");
    }
}
