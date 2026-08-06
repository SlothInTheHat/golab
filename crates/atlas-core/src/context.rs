//! Orientation, assembled once instead of rediscovered per prompt.
//!
//! A coding agent handed "T4: add refund support" otherwise spends its first
//! several turns grepping for things the runtime already knows. The graph knows
//! what calls the symbols in scope and which tests reach them; the lease table
//! knows who is working next door; `memory` holds decisions the team already
//! made; the request log holds every "this moved under you" notice. None of
//! that is new information — it is just never handed over in one piece.
//!
//! Nothing here computes anything new. Every field is an existing `Store`
//! method, composed and then **capped**: this packet goes into a model's
//! context window on every task start, so an uncapped `impact` on a hub symbol
//! would quietly cost more than the work it is describing.

use std::collections::HashSet;

use anyhow::{anyhow, Result};
use serde::Serialize;

use crate::graph::{ImpactNode, Neighbors};
use crate::model::*;
use crate::protocol::Direction;
use crate::schedule::ScheduledTask;
use crate::session::SessionView;
use crate::store::Store;
use crate::work::{AgentView, TaskView};

/// Caps. Generous enough to be useful, small enough that a hub symbol in a
/// large repository cannot blow up the packet.
const MAX_IMPACT: usize = 40;
const MAX_EVENTS: usize = 20;
const MAX_MEMORY: usize = 20;
const MAX_TESTS: usize = 15;

/// Tags that mark a memory entry as worth reading before touching code.
const STANDING_TAGS: [&str; 5] = [
    "architecture",
    "convention",
    "decision",
    "interface",
    "standard",
];

#[derive(Debug, Clone, Serialize)]
pub struct ScopedSymbol {
    pub symbol: Symbol,
    /// Whoever holds it — including you. `None` means nobody.
    pub lease: Option<Lease>,
    /// `GET /payments/{id}` when this is an endpoint.
    pub route: Option<String>,
    /// One hop out: who calls it, what it calls, what is nested inside it.
    pub neighbors: Neighbors,
}

/// Everything worth knowing before starting one task.
#[derive(Debug, Clone, Serialize)]
pub struct TaskContext {
    pub task: TaskView,
    pub goal: Option<Goal>,
    pub goal_progress: Option<GoalProgress>,
    pub scope: Vec<ScopedSymbol>,
    /// Blast radius, annotated with who holds what.
    pub impact: Vec<ImpactNode>,
    /// Tests reaching anything in scope — where a regression shows up first.
    pub tests: Vec<Symbol>,
    /// Decisions and conventions that touch this scope.
    pub memory: Vec<MemoryEntry>,
    /// Open api-change notices and other live requests for the assignee.
    pub notices: Vec<Request>,
    pub blocked_by: Vec<TaskView>,
    pub blocks: Vec<TaskView>,
    /// Who else is working inside the blast radius right now. The answer to
    /// "am I about to collide with someone", before the collision.
    pub neighbors_at_work: Vec<AgentView>,
    pub recent_events: Vec<Event>,
}

/// Orientation for an agent that has no current task: what the workspace
/// wants, and what it could pick up.
#[derive(Debug, Clone, Serialize)]
pub struct AgentContext {
    pub agent: AgentView,
    pub sessions: Vec<SessionView>,
    pub open_goals: Vec<GoalWithProgress>,
    /// Wave 0, filtered to what is actually safe to start.
    pub startable: Vec<ScheduledTask>,
    pub critical_path: Vec<TaskView>,
    pub notices: Vec<Request>,
    pub memory: Vec<MemoryEntry>,
    pub held: Vec<Lease>,
    /// Present when the agent is mid-task, so one call orients either way.
    pub task: Option<Box<TaskContext>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct GoalWithProgress {
    #[serde(flatten)]
    pub goal: Goal,
    pub progress: GoalProgress,
}

impl Store {
    /// Everything an agent needs to start `task_id` without re-reading the
    /// repository.
    pub fn task_context(&self, task_id: &str, depth: usize) -> Result<TaskContext> {
        let task = self
            .task(task_id)?
            .ok_or_else(|| anyhow!("no such task: {task_id}"))?;

        let goal_id = self.task_goal(task_id)?;
        let goal = match &goal_id {
            Some(g) => self.goal(g)?,
            None => None,
        };
        let goal_progress = match (&goal_id, &goal) {
            (Some(g), Some(_)) => Some(self.goal_progress(g)?),
            _ => None,
        };

        let leases = self.active_leases(None)?;
        let scope_symbols = self.task_scope(task_id)?;
        let mut scope = Vec::with_capacity(scope_symbols.len());
        for s in &scope_symbols {
            scope.push(ScopedSymbol {
                lease: leases.iter().find(|l| l.symbol_id == s.id).cloned(),
                route: s.route(),
                neighbors: self.neighbors(&s.id)?,
                symbol: s.clone(),
            });
        }

        // One blast radius for the whole scope, not one per symbol: the
        // nearest distance wins, because that is the one that matters.
        let mut impact: Vec<ImpactNode> = Vec::new();
        let scope_ids: HashSet<&str> = scope_symbols.iter().map(|s| s.id.as_str()).collect();
        for s in &scope_symbols {
            for node in self.impact(&s.id, depth)? {
                if scope_ids.contains(node.symbol.id.as_str()) {
                    continue;
                }
                match impact.iter_mut().find(|n| n.symbol.id == node.symbol.id) {
                    Some(existing) => existing.distance = existing.distance.min(node.distance),
                    None => impact.push(node),
                }
            }
        }
        impact.sort_by(|a, b| {
            a.distance
                .cmp(&b.distance)
                .then_with(|| a.symbol.path.cmp(&b.symbol.path))
        });
        impact.truncate(MAX_IMPACT);

        let tests = self.tests_reaching(&scope)?;
        let memory = self.memory_near(&scope_symbols)?;

        let assignee = task.task.assignee.clone();
        let notices = match &assignee {
            Some(a) => self.notices_for(a, task_id, &scope_symbols, &impact)?,
            None => Vec::new(),
        };

        let all = self.tasks()?;
        let blocked_by: Vec<TaskView> = all
            .iter()
            .filter(|t| task.task.deps.contains(&t.task.id))
            .cloned()
            .collect();
        let blocks: Vec<TaskView> = all
            .iter()
            .filter(|t| t.task.deps.contains(&task.task.id))
            .cloned()
            .collect();

        let radius: HashSet<&str> = scope_symbols
            .iter()
            .map(|s| s.id.as_str())
            .chain(impact.iter().map(|n| n.symbol.id.as_str()))
            .collect();
        let neighbors_at_work: Vec<AgentView> = self
            .agents()?
            .into_iter()
            .filter(|a| {
                a.agent.name != assignee.clone().unwrap_or_default()
                    && leases
                        .iter()
                        .any(|l| l.agent == a.agent.name && radius.contains(l.symbol_id.as_str()))
            })
            .collect();

        let handles: HashSet<String> = scope_symbols
            .iter()
            .map(|s| s.handle())
            .chain(impact.iter().map(|n| n.symbol.handle()))
            .collect();
        let recent_events: Vec<Event> = self
            .recent_events(200)?
            .into_iter()
            .filter(|e| {
                e.task.as_deref() == Some(task_id)
                    || e.symbol_handle
                        .as_deref()
                        .is_some_and(|h| handles.contains(h))
            })
            .take(MAX_EVENTS)
            .collect();

        Ok(TaskContext {
            task,
            goal,
            goal_progress,
            scope,
            impact,
            tests,
            memory,
            notices,
            blocked_by,
            blocks,
            neighbors_at_work,
            recent_events,
        })
    }

    /// Orientation for an agent rather than a task. When the agent is already
    /// working, its task context comes along, so a caller never has to decide
    /// which of the two questions to ask.
    pub fn agent_context(&self, agent: &str, depth: usize) -> Result<AgentContext> {
        let view = self
            .agents()?
            .into_iter()
            .find(|a| a.agent.name == agent)
            .ok_or_else(|| anyhow!("no such agent: {agent}"))?;

        let plan = self.plan()?;
        let tasks = self.tasks()?;
        let startable: Vec<ScheduledTask> = plan
            .waves
            .first()
            .map(|w| w.tasks.iter().filter(|t| t.startable()).cloned().collect())
            .unwrap_or_default();
        let critical_path: Vec<TaskView> = plan
            .critical_path
            .iter()
            .filter_map(|id| tasks.iter().find(|t| &t.task.id == id).cloned())
            .collect();

        let mut open_goals = Vec::new();
        for goal in self.goals()? {
            if goal.state == GoalState::Open {
                let progress = self.goal_progress(&goal.id)?;
                open_goals.push(GoalWithProgress { goal, progress });
            }
        }

        let task = match &view.current_task {
            Some(id) => Some(Box::new(self.task_context(id, depth)?)),
            None => None,
        };

        Ok(AgentContext {
            sessions: self.sessions_for(agent)?,
            open_goals,
            startable,
            critical_path,
            notices: self.requests(Some(agent), Direction::Inbox, true)?,
            // With no task to narrow by, standing decisions are the useful
            // half — an agent about to pick up work wants the conventions.
            memory: self.standing_memory()?,
            held: self.active_leases(Some(agent))?,
            task,
            agent: view,
        })
    }

    /// Tests that exercise anything in scope, by either signal the index
    /// carries: an explicit `tests` edge, or a caller the role detector
    /// already recognised as a test.
    fn tests_reaching(&self, scope: &[ScopedSymbol]) -> Result<Vec<Symbol>> {
        let mut out: Vec<Symbol> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        for s in scope {
            for caller in &s.neighbors.callers {
                let is_test =
                    caller.kind == EdgeKind::Tests || caller.symbol.role == Some(Role::Test);
                if is_test && seen.insert(caller.symbol.id.clone()) {
                    out.push(caller.symbol.clone());
                }
            }
        }
        out.truncate(MAX_TESTS);
        Ok(out)
    }

    /// Memory entries worth reading before touching this scope.
    ///
    /// A heuristic, deliberately: either the entry is tagged as a standing
    /// decision, or its key or value names something in scope. Widening this
    /// costs context window on every task start, so it errs towards quiet.
    fn memory_near(&self, scope: &[Symbol]) -> Result<Vec<MemoryEntry>> {
        let needles: Vec<String> = scope
            .iter()
            .flat_map(|s| [s.name.to_lowercase(), s.path.to_lowercase()])
            .filter(|n| n.len() >= 3)
            .collect();

        let mut out: Vec<MemoryEntry> = self
            .memory_list(None)?
            .into_iter()
            .filter(|m| {
                let standing = m
                    .tags
                    .iter()
                    .any(|t| STANDING_TAGS.contains(&t.to_lowercase().as_str()));
                let hay = format!("{} {}", m.key, m.value).to_lowercase();
                standing || needles.iter().any(|n| hay.contains(n))
            })
            .collect();
        out.sort_by_key(|m| std::cmp::Reverse(m.updated_at));
        out.truncate(MAX_MEMORY);
        Ok(out)
    }

    fn standing_memory(&self) -> Result<Vec<MemoryEntry>> {
        let mut out: Vec<MemoryEntry> = self
            .memory_list(None)?
            .into_iter()
            .filter(|m| {
                m.tags
                    .iter()
                    .any(|t| STANDING_TAGS.contains(&t.to_lowercase().as_str()))
            })
            .collect();
        out.sort_by_key(|m| std::cmp::Reverse(m.updated_at));
        out.truncate(MAX_MEMORY);
        Ok(out)
    }

    /// Live requests for the assignee that bear on this task: anything about
    /// the task itself, about a symbol in or near its scope, or an api-change
    /// notice (which is the runtime telling them the ground moved).
    fn notices_for(
        &self,
        assignee: &str,
        task_id: &str,
        scope: &[Symbol],
        impact: &[ImpactNode],
    ) -> Result<Vec<Request>> {
        let relevant: HashSet<&str> = scope
            .iter()
            .map(|s| s.id.as_str())
            .chain(impact.iter().map(|n| n.symbol.id.as_str()))
            .collect();
        Ok(self
            .requests(Some(assignee), Direction::Inbox, true)?
            .into_iter()
            .filter(|r| {
                r.kind == request_kind::API_CHANGE
                    || r.resource_task.as_deref() == Some(task_id)
                    || r.task.as_deref() == Some(task_id)
                    || r.resource_symbol
                        .as_deref()
                        .is_some_and(|s| relevant.contains(s))
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::DEFAULT_REPO_ID;
    use crate::lease::AcquireOptions;
    use crate::protocol::NewRequest;
    use serde_json::json;

    fn fixture() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("src/pay.ts"),
            "export function charge(x: number) { return x; }\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("src/checkout.ts"),
            "import { charge } from './pay';\n\
             export function checkout(x: number) { return charge(x); }\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("src/pay.test.ts"),
            "import { charge } from './pay';\n\
             test('charges', () => { charge(1); });\n",
        )
        .unwrap();
        let mut store = Store::init(dir.path()).unwrap();
        crate::scan::scan(&mut store, DEFAULT_REPO_ID, dir.path(), &[], false).unwrap();
        store.register_agent("alice", "claude-code").unwrap();
        store.register_agent("bob", "cursor").unwrap();
        (dir, store)
    }

    /// A task scoped to `charge`, claimed by alice.
    fn claimed_task(store: &mut Store) -> String {
        let t = store.add_task("rework charging", 9, &[]).unwrap();
        store
            .set_task_scope(&t.id, &["src/pay.ts:charge".to_string()])
            .unwrap();
        store.claim_next("alice", 300).unwrap().unwrap();
        t.id
    }

    #[test]
    fn a_task_context_names_the_scope_and_who_holds_it() {
        let (_d, mut store) = fixture();
        let id = claimed_task(&mut store);

        let ctx = store.task_context(&id, 2).unwrap();
        assert_eq!(ctx.task.task.id, id);
        assert_eq!(ctx.scope.len(), 1);
        assert_eq!(ctx.scope[0].symbol.name, "charge");
        assert_eq!(
            ctx.scope[0].lease.as_ref().map(|l| l.agent.as_str()),
            Some("alice"),
            "the packet must say the scope is already leased to you"
        );
    }

    #[test]
    fn the_blast_radius_names_the_caller() {
        let (_d, mut store) = fixture();
        let id = claimed_task(&mut store);

        let ctx = store.task_context(&id, 2).unwrap();
        assert!(
            ctx.impact.iter().any(|n| n.symbol.name == "checkout"),
            "impact should reach the caller: {:?}",
            ctx.impact.iter().map(|n| &n.symbol.name).collect::<Vec<_>>()
        );
        assert!(
            !ctx.impact.iter().any(|n| n.symbol.name == "charge"),
            "the scope is not its own blast radius"
        );
    }

    #[test]
    fn tests_touching_the_scope_come_along() {
        let (_d, mut store) = fixture();
        let id = claimed_task(&mut store);

        let ctx = store.task_context(&id, 2).unwrap();
        assert!(
            ctx.tests.iter().any(|t| t.path.contains("pay.test.ts")),
            "where a regression shows up first is worth knowing up front: {:?}",
            ctx.tests.iter().map(|t| &t.path).collect::<Vec<_>>()
        );
    }

    #[test]
    fn standing_decisions_and_scope_specific_notes_both_surface() {
        let (_d, mut store) = fixture();
        let id = claimed_task(&mut store);
        store
            .memory_set("payments-are-idempotent", "always", Some("alice"), &["architecture".into()])
            .unwrap();
        store
            .memory_set("charge-rounding", "round half up", Some("alice"), &[])
            .unwrap();
        store
            .memory_set("unrelated-thing", "nothing to do with it", Some("bob"), &[])
            .unwrap();

        let ctx = store.task_context(&id, 2).unwrap();
        let keys: Vec<&str> = ctx.memory.iter().map(|m| m.key.as_str()).collect();
        assert!(keys.contains(&"payments-are-idempotent"), "tagged: {keys:?}");
        assert!(keys.contains(&"charge-rounding"), "names the symbol: {keys:?}");
        assert!(!keys.contains(&"unrelated-thing"), "quiet by default: {keys:?}");
    }

    #[test]
    fn an_api_change_notice_reaches_the_assignee() {
        let (_d, mut store) = fixture();
        let id = claimed_task(&mut store);
        store
            .open_request(&NewRequest {
                to: Some("alice".to_string()),
                body: json!({ "note": "this moved under you" }),
                ..NewRequest::new(request_kind::API_CHANGE, "atlas", "checkout changed")
            })
            .unwrap();

        let ctx = store.task_context(&id, 2).unwrap();
        assert_eq!(ctx.notices.len(), 1);
        assert_eq!(ctx.notices[0].kind, request_kind::API_CHANGE);
    }

    #[test]
    fn whoever_is_working_next_door_is_named() {
        let (_d, mut store) = fixture();
        let id = claimed_task(&mut store);
        store
            .acquire_ref("checkout", "bob", &AcquireOptions::default())
            .unwrap();

        let ctx = store.task_context(&id, 2).unwrap();
        assert_eq!(
            ctx.neighbors_at_work
                .iter()
                .map(|a| a.agent.name.as_str())
                .collect::<Vec<_>>(),
            vec!["bob"],
            "bob holds a caller of my scope — that is a collision waiting to happen"
        );
    }

    #[test]
    fn dependencies_are_reported_in_both_directions() {
        let (_d, mut store) = fixture();
        let first = store.add_task("first", 5, &[]).unwrap();
        let second = store
            .add_task("second", 5, std::slice::from_ref(&first.id))
            .unwrap();

        let ctx = store.task_context(&second.id, 2).unwrap();
        assert_eq!(ctx.blocked_by.len(), 1);
        assert_eq!(ctx.blocked_by[0].task.id, first.id);

        let first_ctx = store.task_context(&first.id, 2).unwrap();
        assert_eq!(first_ctx.blocks[0].task.id, second.id);
    }

    #[test]
    fn an_idle_agent_gets_goals_and_what_it_could_start() {
        let (_d, mut store) = fixture();
        let g = store.add_goal("ship refunds", 9, None, None).unwrap();
        let t = store.goal_decompose(&g.id, "wire it", 9, &[], &[]).unwrap();

        let ctx = store.agent_context("alice", 2).unwrap();
        assert!(ctx.task.is_none(), "alice has no task yet");
        assert_eq!(ctx.open_goals.len(), 1);
        assert_eq!(ctx.open_goals[0].goal.id, g.id);
        assert_eq!(ctx.startable.len(), 1);
        assert_eq!(ctx.startable[0].task.task.id, t.id);
    }

    #[test]
    fn a_working_agent_gets_its_task_context_in_the_same_call() {
        let (_d, mut store) = fixture();
        let id = claimed_task(&mut store);

        let ctx = store.agent_context("alice", 2).unwrap();
        let task = ctx.task.expect("mid-task agents carry their task context");
        assert_eq!(task.task.task.id, id);
        assert_eq!(ctx.held.len(), 1, "and what it is holding while it works");
    }

    #[test]
    fn a_live_session_shows_up_in_the_agents_context() {
        let (_d, mut store) = fixture();
        store
            .open_session(&crate::session::NewSession::new(
                "alice",
                "claude-code",
                crate::session::transport::MCP,
                "/repo",
            ))
            .unwrap();

        let ctx = store.agent_context("alice", 2).unwrap();
        assert_eq!(ctx.sessions.len(), 1);
        assert!(ctx.sessions[0].live);
    }

    #[test]
    fn the_packet_stays_capped() {
        let (_d, mut store) = fixture();
        let id = claimed_task(&mut store);
        for i in 0..60 {
            store
                .memory_set(&format!("note-{i}"), "x", Some("alice"), &["decision".into()])
                .unwrap();
        }

        let ctx = store.task_context(&id, 4).unwrap();
        assert!(ctx.memory.len() <= MAX_MEMORY, "this rides in a context window");
        assert!(ctx.impact.len() <= MAX_IMPACT);
        assert!(ctx.recent_events.len() <= MAX_EVENTS);
    }
}
