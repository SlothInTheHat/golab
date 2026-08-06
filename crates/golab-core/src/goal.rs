//! Goals: what a human actually asked for.
//!
//! Everything below this module — tasks, leases, the knowledge graph — exists
//! to serve a goal, but none of it names one. A task is "touch `voidPayment`
//! and `record`"; a goal is "implement refunds." This module is the thin
//! layer that lets an agent or a human say the second sentence and have it
//! decompose into the first kind of thing.
//!
//! Decomposition here is explicit, not generated: `goal_decompose` is the API
//! a human or an agent calls once they have decided what the subtasks are.
//! `suggest_decomposition` is the one assist the knowledge graph can honestly
//! offer without a model in the loop — it proposes *where* a goal's impact
//! lands (clustered by service), not *what* to do about it.

use std::collections::{BTreeMap, HashSet};

use anyhow::{anyhow, Result};
use rusqlite::{params, OptionalExtension, Row};
use serde_json::json;

use crate::ids;
use crate::model::*;
use crate::store::{self, Store};
use crate::work::TaskView;

impl Store {
    /// Name a goal. Ids follow the same convention as tasks (`G1`, `G2`, ...).
    pub fn add_goal(
        &mut self,
        title: &str,
        priority: i64,
        description: Option<&str>,
        created_by: Option<&str>,
    ) -> Result<Goal> {
        let title = title.to_string();
        let description = description.map(|s| s.to_string());
        let created_by = created_by.map(|s| s.to_string());
        self.write(move |tx| {
            let n: i64 = tx.query_row("SELECT COUNT(*) FROM goals", [], |r| r.get(0))?;
            let id = format!("G{}", n + 1);
            let now = ids::now_ms();
            tx.execute(
                "INSERT INTO goals(id, title, description, state, priority, created_at, \
                     updated_at, created_by) \
                 VALUES (?1, ?2, ?3, 'open', ?4, ?5, ?5, ?6)",
                params![id, title, description, priority, now, created_by],
            )?;
            store::emit(
                tx,
                "goal.created",
                created_by.as_deref(),
                None,
                None,
                json!({ "goal": id, "title": title, "priority": priority }),
            )?;
            Ok(Goal {
                id,
                title,
                description,
                state: GoalState::Open,
                priority,
                created_at: now,
                updated_at: now,
                created_by,
            })
        })
    }

    pub fn goals(&self) -> Result<Vec<Goal>> {
        let mut stmt = self.conn().prepare(
            "SELECT id, title, description, state, priority, created_at, updated_at, created_by \
             FROM goals ORDER BY priority DESC, created_at",
        )?;
        let rows = stmt.query_map([], row_to_goal)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    pub fn goal(&self, id: &str) -> Result<Option<Goal>> {
        Ok(self
            .conn()
            .query_row(
                "SELECT id, title, description, state, priority, created_at, updated_at, created_by \
                 FROM goals WHERE id = ?1",
                params![id],
                row_to_goal,
            )
            .optional()?)
    }

    pub fn set_goal_state(&mut self, id: &str, state: GoalState) -> Result<Goal> {
        let id_owned = id.to_string();
        self.write(move |tx| {
            let n = tx.execute(
                "UPDATE goals SET state = ?2, updated_at = ?3 WHERE id = ?1",
                params![id_owned, state.as_str(), ids::now_ms()],
            )?;
            if n == 0 {
                return Err(anyhow!("no such goal: {id_owned}"));
            }
            store::emit(
                tx,
                "goal.state_changed",
                None,
                None,
                None,
                json!({ "goal": id_owned, "state": state.as_str() }),
            )?;
            Ok(())
        })?;
        self.goal(id)?.ok_or_else(|| anyhow!("goal vanished: {id}"))
    }

    /// Declare a subtask under a goal. Thin wrapper: creates the task, scopes
    /// it, and links it — no new task-graph logic, everything below this call
    /// is the existing scheduler.
    pub fn goal_decompose(
        &mut self,
        goal_id: &str,
        title: &str,
        priority: i64,
        deps: &[String],
        symbols: &[String],
    ) -> Result<Task> {
        if self.goal(goal_id)?.is_none() {
            return Err(anyhow!("no such goal: {goal_id}"));
        }
        let task = self.add_task(title, priority, deps)?;
        if !symbols.is_empty() {
            self.set_task_scope(&task.id, symbols)?;
        }
        let task_id = task.id.clone();
        let goal_id_owned = goal_id.to_string();
        self.write(move |tx| {
            tx.execute(
                "INSERT OR REPLACE INTO task_goals(task_id, goal_id) VALUES (?1, ?2)",
                params![task_id, goal_id_owned],
            )?;
            store::emit(
                tx,
                "goal.decomposed",
                None,
                None,
                Some(&task_id),
                json!({ "goal": goal_id_owned }),
            )?;
            Ok(())
        })?;
        Ok(task)
    }

    /// The tasks serving a goal, in the same order `tasks()` reports them.
    pub fn goal_tasks(&self, goal_id: &str) -> Result<Vec<TaskView>> {
        let mut stmt = self
            .conn()
            .prepare("SELECT task_id FROM task_goals WHERE goal_id = ?1")?;
        let ids: HashSet<String> = stmt
            .query_map(params![goal_id], |r| r.get::<_, String>(0))?
            .collect::<Result<_, _>>()?;
        Ok(self
            .tasks()?
            .into_iter()
            .filter(|t| ids.contains(&t.task.id))
            .collect())
    }

    /// The goal a task serves, if any.
    /// Every goal with its rollup already attached.
    ///
    /// The dashboard used to fetch `/api/goals` and then one
    /// `/api/goals/{id}/progress` per goal, which meant the panel rendered
    /// twice and could sit half-empty while the second wave was in flight.
    /// Goals are few; doing the join here costs one pass and removes N
    /// requests from every client.
    pub fn goals_with_progress(&self) -> Result<Vec<crate::context::GoalWithProgress>> {
        let mut out = Vec::new();
        for goal in self.goals()? {
            let progress = self.goal_progress(&goal.id)?;
            out.push(crate::context::GoalWithProgress { goal, progress });
        }
        Ok(out)
    }

    pub fn task_goal(&self, task_id: &str) -> Result<Option<String>> {
        Ok(self
            .conn()
            .query_row(
                "SELECT goal_id FROM task_goals WHERE task_id = ?1",
                params![task_id],
                |r| r.get(0),
            )
            .optional()?)
    }

    pub fn goal_progress(&self, goal_id: &str) -> Result<GoalProgress> {
        if self.goal(goal_id)?.is_none() {
            return Err(anyhow!("no such goal: {goal_id}"));
        }
        let tasks = self.goal_tasks(goal_id)?;
        let total = tasks.len() as i64;
        let count = |s: TaskState| tasks.iter().filter(|t| t.task.state == s).count() as i64;
        let done = count(TaskState::Done);
        let running = count(TaskState::Running);
        let in_review = tasks
            .iter()
            .filter(|t| t.task.state.as_str() == "review")
            .count() as i64;
        let pending = count(TaskState::Pending) + count(TaskState::Blocked);
        let percent = if total == 0 {
            0.0
        } else {
            (done as f64 / total as f64) * 100.0
        };
        let mut contributing_agents: Vec<String> = tasks
            .iter()
            .filter_map(|t| t.task.assignee.clone())
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        contributing_agents.sort();
        Ok(GoalProgress {
            total,
            done,
            running,
            in_review,
            pending,
            percent,
            contributing_agents,
        })
    }

    /// Propose a task breakdown by clustering the impact radius of a symbol
    /// by the service it lands in. Advisory: names *where* work touches, not
    /// what the work is — there is no model here to invent that.
    pub fn suggest_decomposition(
        &self,
        near_symbol: &str,
        depth: usize,
    ) -> Result<Vec<TaskSuggestion>> {
        let origin = self.resolve(near_symbol)?;
        cluster_by_service(self, &[origin], depth)
    }

    /// The fully-automatic sibling of `suggest_decomposition`: instead of a
    /// human naming a `--near` symbol, the goal's own title and description
    /// are keyword-matched against the graph to find candidate entry points,
    /// then clustered exactly the same way. Still advisory, and still not
    /// generative — there is no model here, only a text search. Returns an
    /// empty list (not an error) when nothing in the graph resembles the
    /// goal's wording; the caller should point the human at `--near` instead.
    pub fn plan_goal(&self, goal_id: &str, depth: usize) -> Result<Vec<TaskSuggestion>> {
        let goal = self.goal(goal_id)?.ok_or_else(|| anyhow!("no such goal: {goal_id}"))?;
        let text = match &goal.description {
            Some(d) => format!("{} {d}", goal.title),
            None => goal.title.clone(),
        };
        let seeds = self.keyword_match_symbols(&text)?;
        if seeds.is_empty() {
            return Ok(Vec::new());
        }
        cluster_by_service(self, &seeds, depth)
    }

    /// Every symbol whose name, qualified name or path contains a word from
    /// `text` — reusing the exact substring search that backs `golab symbols
    /// <pattern>`, not new search logic. Tokens are matched both as written
    /// and with a naive trailing `s`/`es` stripped, so a goal titled
    /// "Implement refunds" still finds a function named `refund`.
    fn keyword_match_symbols(&self, text: &str) -> Result<Vec<Symbol>> {
        const STOPWORDS: &[&str] = &[
            "the", "a", "an", "implement", "add", "support", "for", "and", "to", "of", "in", "on",
            "with", "build", "create", "make", "new",
        ];
        let mut seen: HashSet<String> = HashSet::new();
        let mut out = Vec::new();
        for raw in text.split(|c: char| !c.is_alphanumeric()) {
            let token = raw.to_lowercase();
            if token.len() < 3 || STOPWORDS.contains(&token.as_str()) {
                continue;
            }
            let mut forms = vec![token.clone()];
            if let Some(s) = token.strip_suffix("es") {
                forms.push(s.to_string());
            } else if let Some(s) = token.strip_suffix('s') {
                forms.push(s.to_string());
            }
            for form in forms {
                for sym in self.list_symbols(None, None, Some(&form), 50)? {
                    if seen.insert(sym.id.clone()) {
                        out.push(sym);
                    }
                }
            }
        }
        Ok(out)
    }
}

/// Cluster `seeds` plus each one's impact radius by owning service, the
/// shared implementation behind both `suggest_decomposition` (one seed, from
/// `--near`) and `plan_goal` (many seeds, from a keyword match).
fn cluster_by_service(store: &Store, seeds: &[Symbol], depth: usize) -> Result<Vec<TaskSuggestion>> {
    let mut groups: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut assign = |label: String, handle: String| {
        groups.entry(label).or_default().push(handle);
    };

    for origin in seeds {
        // Each seed always seeds its own group.
        assign(group_label(store, origin)?, origin.handle());
        for node in store.impact(&origin.id, depth)? {
            assign(group_label(store, &node.symbol)?, node.symbol.handle());
        }
    }

    Ok(groups
        .into_iter()
        .map(|(label, mut symbols)| {
            symbols.sort();
            symbols.dedup();
            TaskSuggestion {
                title: format!("Update {label}"),
                symbols,
            }
        })
        .collect())
}

/// The nearest enclosing service name for a symbol, falling back to its file
/// path when it belongs to no service (e.g. a script outside any manifest).
fn group_label(store: &Store, symbol: &Symbol) -> Result<String> {
    for ancestor in store.ancestors(&symbol.id)? {
        if ancestor.kind == SymbolKind::Service {
            return Ok(ancestor.name);
        }
    }
    Ok(symbol.path.clone())
}

fn row_to_goal(r: &Row) -> rusqlite::Result<Goal> {
    let state: String = r.get(3)?;
    Ok(Goal {
        id: r.get(0)?,
        title: r.get(1)?,
        description: r.get(2)?,
        state: GoalState::parse(&state).unwrap_or(GoalState::Open),
        priority: r.get(4)?,
        created_at: r.get(5)?,
        updated_at: r.get(6)?,
        created_by: r.get(7)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan;

    fn fixture() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("api/src")).unwrap();
        std::fs::create_dir_all(dir.path().join("lib/src")).unwrap();
        std::fs::write(dir.path().join("api/package.json"), r#"{"name":"payments-api"}"#).unwrap();
        std::fs::write(dir.path().join("lib/package.json"), r#"{"name":"ledger-lib"}"#).unwrap();
        std::fs::write(
            dir.path().join("lib/src/ledger.ts"),
            "export function record(x: number) { return x; }\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("api/src/routes.ts"),
            "import { record } from '../../lib/src/ledger';\n\
             export function createPayment(x: number) { return record(x); }\n\
             export function refund(x: number) { return -x; }\n",
        )
        .unwrap();
        let mut store = Store::init(dir.path()).unwrap();
        scan::scan(&mut store, crate::ids::DEFAULT_REPO_ID, dir.path(), &[], false).unwrap();
        (dir, store)
    }

    #[test]
    fn goal_decompose_creates_scoped_tasks_linked_to_the_goal() {
        let (_d, mut store) = fixture();
        let goal = store.add_goal("Implement refunds", 5, None, Some("alice")).unwrap();
        assert_eq!(goal.id, "G1");
        assert_eq!(goal.state, GoalState::Open);

        let t1 = store
            .goal_decompose(&goal.id, "wire the endpoint", 5, &[], &["createPayment".to_string()])
            .unwrap();
        let t2 = store
            .goal_decompose(&goal.id, "write the ledger", 3, &[], &["record".to_string()])
            .unwrap();

        assert_eq!(store.task_goal(&t1.id).unwrap().as_deref(), Some(goal.id.as_str()));
        let tasks = store.goal_tasks(&goal.id).unwrap();
        let ids: Vec<&str> = tasks.iter().map(|t| t.task.id.as_str()).collect();
        assert!(ids.contains(&t1.id.as_str()) && ids.contains(&t2.id.as_str()));

        // The task's scope is real: it leases like any other scoped task.
        store.register_agent("agent-1", "claude").unwrap();
        let claimed = store.claim_next("agent-1", 300).unwrap().unwrap();
        assert!(!claimed.scope.is_empty());
    }

    #[test]
    fn goal_progress_aggregates_task_states() {
        let (_d, mut store) = fixture();
        let goal = store.add_goal("Implement refunds", 5, None, None).unwrap();
        let t1 = store
            .goal_decompose(&goal.id, "a", 5, &[], &["createPayment".to_string()])
            .unwrap();
        store
            .goal_decompose(&goal.id, "b", 3, &[], &["record".to_string()])
            .unwrap();

        let progress = store.goal_progress(&goal.id).unwrap();
        assert_eq!(progress.total, 2);
        assert_eq!(progress.done, 0);
        assert_eq!(progress.pending, 2);

        store.register_agent("agent-1", "claude").unwrap();
        store
            .set_task_state(&t1.id, TaskState::Done, None, None, true)
            .unwrap();
        let progress = store.goal_progress(&goal.id).unwrap();
        assert_eq!(progress.done, 1);
        assert_eq!(progress.pending, 1);
        assert_eq!(progress.percent, 50.0);
    }

    #[test]
    fn an_unknown_goal_is_rejected() {
        let (_d, mut store) = fixture();
        assert!(store.goal_decompose("G99", "x", 0, &[], &[]).is_err());
        assert!(store.goal_progress("G99").is_err());
        assert!(store.set_goal_state("G99", GoalState::Done).is_err());
    }

    #[test]
    fn suggest_decomposition_groups_impact_by_service() {
        let (_d, store) = fixture();
        let suggestions = store.suggest_decomposition("record", 3).unwrap();
        let titles: Vec<&str> = suggestions.iter().map(|s| s.title.as_str()).collect();
        assert!(titles.contains(&"Update ledger-lib"), "{titles:?}");
        assert!(titles.contains(&"Update payments-api"), "{titles:?}");

        let api_group = suggestions
            .iter()
            .find(|s| s.title == "Update payments-api")
            .unwrap();
        assert!(
            api_group.symbols.iter().any(|s| s.contains("createPayment")),
            "{:?}",
            api_group.symbols
        );
    }

    /// The motivating case for plural-stripping: the fixture's function is
    /// named `refund` (singular); a goal titled "Implement refunds" must
    /// still find it, even though "refund".contains("refunds") is false.
    #[test]
    fn plan_finds_symbols_by_keyword_even_when_the_title_is_plural() {
        let (_d, mut store) = fixture();
        let goal = store.add_goal("Implement refunds", 9, None, None).unwrap();

        let suggestions = store.plan_goal(&goal.id, 2).unwrap();
        assert!(
            suggestions.iter().any(|s| s.symbols.iter().any(|h| h.contains("refund"))),
            "{suggestions:?}"
        );
    }

    #[test]
    fn plan_with_no_keyword_matches_returns_empty_not_an_error() {
        let (_d, mut store) = fixture();
        let goal = store.add_goal("Improve team morale", 9, None, None).unwrap();
        assert!(store.plan_goal(&goal.id, 2).unwrap().is_empty());
    }

    #[test]
    fn plan_on_an_unknown_goal_is_rejected() {
        let (_d, store) = fixture();
        assert!(store.plan_goal("G99", 2).is_err());
    }

    #[test]
    fn plan_apply_creates_real_scoped_tasks() {
        let (_d, mut store) = fixture();
        let goal = store.add_goal("Implement refunds", 9, None, None).unwrap();
        let suggestions = store.plan_goal(&goal.id, 2).unwrap();
        assert!(!suggestions.is_empty());

        for s in &suggestions {
            store.goal_decompose(&goal.id, &s.title, 0, &[], &s.symbols).unwrap();
        }
        assert!(!store.goal_tasks(&goal.id).unwrap().is_empty());
    }
}
