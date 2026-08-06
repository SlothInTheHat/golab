//! The repository as a human pictures it.
//!
//! [`graph`](crate::graph) answers questions about one symbol: who calls it,
//! what it reaches. That is the right shape for an agent about to make a
//! change, and the wrong shape for a person trying to understand a codebase —
//! nobody holds two thousand functions in their head. A person thinks in
//! services, subsystems and the database.
//!
//! So this collapses the symbol graph into the picture on a whiteboard:
//! services from manifests, their directories, their files, the tables they
//! query, and the dependencies between them. Every node and edge here is
//! *derived* from what [`scan`](crate::scan) already indexed — nothing new is
//! parsed, and there is no second source of truth about the repository.
//!
//! # It is a live picture, not a diagram
//!
//! The reason to build this at all is the overlays. Each node carries who is
//! working inside it right now ([`activity`](crate::activity)), what they
//! hold, which goals and tasks land there, and what is waiting on review. A
//! static architecture diagram goes stale the day it is drawn; this one cannot,
//! because it is a query.
//!
//! # Depth
//!
//! Depth is how far the collapse goes, and the useful default is shallow:
//!
//! - `1` — services only. A dozen boxes for most repositories.
//! - `2` — plus top-level directories inside each service.
//! - `3` — plus files.
//!
//! Tables are always their own nodes regardless of depth: "the database" is a
//! thing people point at, and burying it inside whichever `.sql` file happens
//! to declare it would hide the one edge everybody wants to trace.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use anyhow::{anyhow, Result};
use rusqlite::params;
use serde::{Deserialize, Serialize};

use crate::activity::ActivityView;
use crate::ids;
use crate::model::*;
use crate::store::Store;
use crate::work::TaskView;

/// Impact radius returned with a node's detail. Same reasoning as
/// `context.rs`: this crosses the wire on every click.
const MAX_IMPACT: usize = 40;
const MAX_SYMBOLS: usize = 50;
const MAX_EVENTS: usize = 20;
/// How far back "changed recently" looks.
const RECENT_MS: i64 = 60 * 60 * 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ArchKind {
    Repo,
    Service,
    Dir,
    File,
    Table,
}

impl ArchKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ArchKind::Repo => "repo",
            ArchKind::Service => "service",
            ArchKind::Dir => "dir",
            ArchKind::File => "file",
            ArchKind::Table => "table",
        }
    }
}

/// One box on the picture.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArchNode {
    /// A symbol id where one exists (service, file, table), or a synthetic
    /// `dir:<repo>:<path>` / `repo:<repo>` handle where one does not.
    pub id: String,
    pub kind: ArchKind,
    pub name: String,
    /// Repo-relative, for everything but a repo node.
    pub path: Option<String>,
    pub repo_id: String,
    pub parent: Option<String>,

    // ------------------------------------------------ what it is made of
    pub files: i64,
    pub symbols: i64,
    pub endpoints: i64,
    pub tests: i64,

    // ------------------------------------------------ what is happening in it
    /// Anyone holding a lease inside it or editing inside it, deduplicated.
    pub workers: Vec<String>,
    pub activity: Vec<ActivityView>,
    pub leases: i64,
    pub tasks: Vec<String>,
    pub goals: Vec<String>,
    pub review_pending: i64,
    /// Events touching this node's files in the last hour.
    pub changed_recently: i64,
}

/// A pointer to another node, carrying enough to render it.
///
/// Bare ids would make every arrow on the picture a second lookup, and a
/// terminal has nowhere to look them up *from* — `s_40b2d47c` is not an answer
/// to "what does this depend on".
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArchRef {
    pub id: String,
    pub name: String,
    pub kind: ArchKind,
}

impl From<&ArchNode> for ArchRef {
    fn from(n: &ArchNode) -> ArchRef {
        ArchRef {
            id: n.id.clone(),
            name: n.name.clone(),
            kind: n.kind,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArchEdge {
    pub from: String,
    pub to: String,
    pub kind: EdgeKind,
    /// How many underlying symbol edges rolled up into this one. A thick
    /// arrow between two services means something different from a thin one.
    pub weight: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArchGraph {
    pub nodes: Vec<ArchNode>,
    pub edges: Vec<ArchEdge>,
    pub depth: usize,
    pub generated_at: i64,
    /// Node ids on a dependency cycle. Reported rather than dropped — a cycle
    /// between two services is a real architectural fact, and silently
    /// breaking the edge to make the layout tidy would hide it.
    pub cycles: Vec<Vec<String>>,
}

/// Everything worth knowing about one box, for the panel under the picture.
#[derive(Debug, Clone, Serialize)]
pub struct NodeDetail {
    pub node: ArchNode,
    /// Nodes this one depends on, and nodes that depend on it — the same two
    /// directions `neighbors` gives for a symbol, one level up.
    pub depends_on: Vec<ArchRef>,
    pub depended_on_by: Vec<ArchRef>,
    pub tables: Vec<ArchRef>,
    pub children: Vec<ArchNode>,
    /// A sample, not the whole subtree — capped at [`MAX_SYMBOLS`].
    pub symbols: Vec<Symbol>,
    pub routes: Vec<Symbol>,
    pub held: Vec<Lease>,
    pub tasks: Vec<TaskView>,
    pub goals: Vec<Goal>,
    pub impact: Vec<crate::graph::ImpactNode>,
    pub recent_events: Vec<Event>,
}

/// Working state while a graph is assembled: which node owns each file.
struct Layout {
    /// repo-relative file path -> node id
    owner: HashMap<String, String>,
    nodes: BTreeMap<String, ArchNode>,
}

impl Store {
    /// The repository, collapsed to `depth`.
    ///
    /// `repo_id` of `None` covers every registered repository, which is what a
    /// dashboard wants; naming one restricts to it.
    pub fn architecture(&self, repo_id: Option<&str>, depth: usize) -> Result<ArchGraph> {
        let depth = depth.clamp(1, 3);
        let mut layout = Layout {
            owner: HashMap::new(),
            nodes: BTreeMap::new(),
        };

        let symbols = self.arch_symbols(repo_id)?;
        let services: Vec<&Symbol> = symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Service)
            .collect();
        let files: Vec<&Symbol> = symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::File)
            .collect();

        // A service's own path is its manifest's directory; a file belongs to
        // the *longest* such prefix, which is what makes a workspace of nested
        // crates come out right.
        let mut service_roots: Vec<(String, String, String)> = services
            .iter()
            .map(|s| (s.repo_id.clone(), dir_of(&s.path), s.id.clone()))
            .collect();
        service_roots.sort_by_key(|(_, p, _)| std::cmp::Reverse(p.len()));

        for s in &services {
            layout.nodes.insert(
                s.id.clone(),
                blank_node(&s.id, ArchKind::Service, &s.name, Some(&s.path), &s.repo_id, None),
            );
        }

        for f in &files {
            let service = service_roots
                .iter()
                .find(|(repo, root, _)| *repo == f.repo_id && under(&f.path, root))
                .map(|(_, _, id)| id.clone());

            // Files with no manifest above them still have to live somewhere,
            // or a repo without a recognised manifest would render empty.
            let service = match service {
                Some(id) => id,
                None => {
                    let id = loose_id(&f.repo_id);
                    layout.nodes.entry(id.clone()).or_insert_with(|| {
                        blank_node(&id, ArchKind::Repo, &f.repo_id, None, &f.repo_id, None)
                    });
                    id
                }
            };

            let owner = match depth {
                1 => service.clone(),
                2 => {
                    let owner = layout.nodes.get(&service);
                    let base = owner
                        .and_then(|n| n.path.as_deref())
                        .map(dir_of)
                        .unwrap_or_default();
                    let service_name = owner.map(|n| n.name.clone()).unwrap_or_default();
                    let dir = top_dir(&f.path, &base);
                    let id = dir_id(&f.repo_id, &dir);
                    // `src` is the name of a directory in every service in the
                    // repository. Qualifying it is the difference between an
                    // arrow that means something and five identical boxes.
                    let name = format!("{service_name}/{}", leaf(&dir));
                    layout.nodes.entry(id.clone()).or_insert_with(|| {
                        blank_node(&id, ArchKind::Dir, &name, Some(&dir), &f.repo_id, Some(&service))
                    });
                    id
                }
                _ => {
                    layout.nodes.insert(
                        f.id.clone(),
                        blank_node(
                            &f.id,
                            ArchKind::File,
                            leaf(&f.path),
                            Some(&f.path),
                            &f.repo_id,
                            Some(&service),
                        ),
                    );
                    f.id.clone()
                }
            };
            layout.owner.insert(key(&f.repo_id, &f.path), owner);
        }

        // Tables are always their own boxes — see the module doc.
        for t in symbols.iter().filter(|s| s.kind == SymbolKind::Table) {
            layout.nodes.insert(
                t.id.clone(),
                blank_node(&t.id, ArchKind::Table, &t.name, Some(&t.path), &t.repo_id, None),
            );
        }

        self.count_contents(&mut layout, &symbols);
        let edges = self.roll_up_edges(&layout, &symbols)?;
        self.apply_overlays(&mut layout)?;

        let nodes: Vec<ArchNode> = layout.nodes.into_values().collect();
        let cycles = find_cycles(&nodes, &edges);
        Ok(ArchGraph {
            nodes,
            edges,
            depth,
            generated_at: ids::now_ms(),
            cycles,
        })
    }

    /// Everything behind one box, for the panel under the picture.
    ///
    /// Composes existing queries rather than adding new ones — the same
    /// approach `context.rs` takes for a task.
    pub fn arch_node(&self, node_id: &str, depth: usize) -> Result<NodeDetail> {
        let graph = self.architecture(None, depth)?;
        let node = graph
            .nodes
            .iter()
            .find(|n| n.id == node_id)
            .cloned()
            .ok_or_else(|| anyhow!("no such node: {node_id}"))?;

        let refer = |id: &str| -> Option<ArchRef> {
            graph.nodes.iter().find(|n| n.id == id).map(ArchRef::from)
        };
        let depends_on = graph
            .edges
            .iter()
            .filter(|e| e.from == node.id && e.kind == EdgeKind::Imports)
            .filter_map(|e| refer(&e.to))
            .collect();
        let depended_on_by = graph
            .edges
            .iter()
            .filter(|e| e.to == node.id && e.kind == EdgeKind::Imports)
            .filter_map(|e| refer(&e.from))
            .collect();
        let tables = graph
            .edges
            .iter()
            .filter(|e| e.from == node.id && e.kind == EdgeKind::Queries)
            .filter_map(|e| refer(&e.to))
            .collect();
        let children = graph
            .nodes
            .iter()
            .filter(|n| n.parent.as_deref() == Some(node.id.as_str()))
            .cloned()
            .collect();

        let paths = self.paths_under(&node)?;
        let symbols = self.symbols_in(&node, &paths, MAX_SYMBOLS)?;
        let routes: Vec<Symbol> = self
            .symbols_with_role(Role::Api)?
            .into_iter()
            .filter(|s| paths.contains(&key(&s.repo_id, &s.path)))
            .collect();

        let held: Vec<Lease> = self
            .active_leases(None)?
            .into_iter()
            .filter(|l| covers(&paths, &l.symbol_handle, &node.repo_id))
            .collect();

        let all_tasks = self.tasks()?;
        let tasks: Vec<TaskView> = all_tasks
            .iter()
            .filter(|t| node.tasks.contains(&t.task.id))
            .cloned()
            .collect();
        let goals: Vec<Goal> = self
            .goals()?
            .into_iter()
            .filter(|g| node.goals.contains(&g.id))
            .collect();

        // Impact is a symbol-level question, so it only has an answer for a
        // node that *is* a symbol. A synthetic directory has no id to walk
        // from, and inventing one would be worse than saying nothing.
        let impact = if node.id.starts_with("s_") {
            self.impact(&node.id, 2)?.into_iter().take(MAX_IMPACT).collect()
        } else {
            Vec::new()
        };

        let recent_events: Vec<Event> = self
            .recent_events(300)?
            .into_iter()
            .filter(|e| match &e.symbol_handle {
                Some(h) => covers(&paths, h, &node.repo_id),
                None => false,
            })
            .rev()
            .take(MAX_EVENTS)
            .collect();

        Ok(NodeDetail {
            node,
            depends_on,
            depended_on_by,
            tables,
            children,
            symbols,
            routes,
            held,
            tasks,
            goals,
            impact,
            recent_events,
        })
    }

    // -------------------------------------------------------------- internals

    fn arch_symbols(&self, repo_id: Option<&str>) -> Result<Vec<Symbol>> {
        let sql = format!(
            "SELECT {} FROM symbols WHERE (?1 IS NULL OR repo_id = ?1) ORDER BY path",
            crate::store::SYMBOL_COLS
        );
        let mut stmt = self.conn().prepare(&sql)?;
        let rows = stmt.query_map(params![repo_id], crate::store::row_to_symbol)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    fn count_contents(&self, layout: &mut Layout, symbols: &[Symbol]) {
        for s in symbols {
            if s.kind == SymbolKind::Service || s.kind == SymbolKind::Table {
                continue;
            }
            let Some(owner) = layout.owner.get(&key(&s.repo_id, &s.path)).cloned() else {
                continue;
            };
            // Roll counts up the whole ancestry, so a service reports what its
            // directories contain rather than zero.
            let mut cur = Some(owner);
            while let Some(id) = cur {
                let Some(node) = layout.nodes.get_mut(&id) else {
                    break;
                };
                if s.kind == SymbolKind::File {
                    node.files += 1;
                } else {
                    node.symbols += 1;
                    match s.role {
                        Some(Role::Api) => node.endpoints += 1,
                        Some(Role::Test) => node.tests += 1,
                        _ => {}
                    }
                }
                cur = node.parent.clone();
            }
        }
    }

    /// Collapse symbol-level `imports` and `queries` edges onto nodes.
    ///
    /// Self-edges are dropped: "api imports api" is true of every service and
    /// tells a reader nothing.
    fn roll_up_edges(&self, layout: &Layout, symbols: &[Symbol]) -> Result<Vec<ArchEdge>> {
        let by_id: HashMap<&str, &Symbol> = symbols.iter().map(|s| (s.id.as_str(), s)).collect();
        let mut stmt = self
            .conn()
            .prepare("SELECT src, dst, kind FROM edges WHERE kind IN ('imports', 'queries')")?;
        let raw: Vec<(String, String, String)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<Result<Vec<_>, _>>()?;

        let owner_of = |id: &str| -> Option<String> {
            let s = by_id.get(id)?;
            // A table is its own node; everything else is found through the
            // file it lives in.
            if s.kind == SymbolKind::Table {
                return layout.nodes.contains_key(&s.id).then(|| s.id.clone());
            }
            layout.owner.get(&key(&s.repo_id, &s.path)).cloned()
        };

        let mut tally: BTreeMap<(String, String, String), i64> = BTreeMap::new();
        for (src, dst, kind) in raw {
            let (Some(from), Some(to)) = (owner_of(&src), owner_of(&dst)) else {
                continue;
            };
            if from == to {
                continue;
            }
            *tally.entry((from, to, kind)).or_insert(0) += 1;
        }

        Ok(tally
            .into_iter()
            .map(|((from, to, kind), weight)| ArchEdge {
                from,
                to,
                kind: EdgeKind::parse(&kind).unwrap_or(EdgeKind::Imports),
                weight,
            })
            .collect())
    }

    /// Who is in each box, right now.
    fn apply_overlays(&self, layout: &mut Layout) -> Result<()> {
        let add = |layout: &mut Layout, path_key: &str, f: &dyn Fn(&mut ArchNode)| {
            let Some(owner) = layout.owner.get(path_key).cloned() else {
                return;
            };
            let mut cur = Some(owner);
            while let Some(id) = cur {
                let Some(node) = layout.nodes.get_mut(&id) else {
                    break;
                };
                f(node);
                cur = node.parent.clone();
            }
        };

        for lease in self.active_leases(None)? {
            let Some(sym) = self.symbol(&lease.symbol_id)? else {
                continue;
            };
            let holder = lease.agent.clone();
            add(layout, &key(&sym.repo_id, &sym.path), &|n: &mut ArchNode| {
                n.leases += 1;
                if !n.workers.contains(&holder) {
                    n.workers.push(holder.clone());
                }
            });
        }

        for view in self.live_activity()? {
            let who = view.activity.agent.clone();
            let v = view.clone();
            let k = key(&view.activity.repo_id, &view.activity.path);
            add(layout, &k, &|n: &mut ArchNode| {
                n.activity.push(v.clone());
                if !n.workers.contains(&who) {
                    n.workers.push(who.clone());
                }
            });
        }

        for task in self.tasks()? {
            let goal = self.task_goal(&task.task.id)?;
            let reviewing = task.task.state == TaskState::Review;
            for sym in self.task_scope(&task.task.id)? {
                let id = task.task.id.clone();
                let goal = goal.clone();
                add(layout, &key(&sym.repo_id, &sym.path), &|n: &mut ArchNode| {
                    if !n.tasks.contains(&id) {
                        n.tasks.push(id.clone());
                        if reviewing {
                            n.review_pending += 1;
                        }
                    }
                    if let Some(g) = &goal {
                        if !n.goals.contains(g) {
                            n.goals.push(g.clone());
                        }
                    }
                });
            }
        }

        // "Changed recently" is a query over the event log, like throughput —
        // not a counter that could drift from what actually happened.
        let cutoff = ids::now_ms() - RECENT_MS;
        let paths: Vec<String> = layout.owner.keys().cloned().collect();
        for e in self.recent_events(500)? {
            if e.ts < cutoff {
                continue;
            }
            let Some(handle) = &e.symbol_handle else {
                continue;
            };
            let file = handle.split(':').next().unwrap_or(handle).to_string();
            let Some(k) = paths.iter().find(|k| k.ends_with(&format!("\0{file}"))) else {
                continue;
            };
            let k = k.clone();
            add(layout, &k, &|n: &mut ArchNode| n.changed_recently += 1);
        }

        Ok(())
    }

    /// Every `repo\0path` key a node covers.
    fn paths_under(&self, node: &ArchNode) -> Result<BTreeSet<String>> {
        let mut out = BTreeSet::new();
        match node.kind {
            ArchKind::Table | ArchKind::File => {
                if let Some(p) = &node.path {
                    out.insert(key(&node.repo_id, p));
                }
            }
            _ => {
                let base = match &node.path {
                    Some(p) if node.kind == ArchKind::Service => dir_of(p),
                    Some(p) => p.clone(),
                    None => String::new(),
                };
                for f in self.arch_symbols(Some(&node.repo_id))? {
                    if f.kind == SymbolKind::File && under(&f.path, &base) {
                        out.insert(key(&f.repo_id, &f.path));
                    }
                }
            }
        }
        Ok(out)
    }

    fn symbols_in(
        &self,
        node: &ArchNode,
        paths: &BTreeSet<String>,
        limit: usize,
    ) -> Result<Vec<Symbol>> {
        Ok(self
            .arch_symbols(Some(&node.repo_id))?
            .into_iter()
            .filter(|s| {
                !matches!(s.kind, SymbolKind::File | SymbolKind::Service)
                    && paths.contains(&key(&s.repo_id, &s.path))
            })
            .take(limit)
            .collect())
    }
}

// ------------------------------------------------------------------ helpers

/// Keys are `repo\0path`, because `path` alone is not unique across a
/// multi-repo workspace — the same trap `ids::symbol_id` hashes `repo_id`
/// first to avoid.
fn key(repo: &str, path: &str) -> String {
    format!("{repo}\0{path}")
}

fn covers(paths: &BTreeSet<String>, handle: &str, repo: &str) -> bool {
    let file = handle.split(':').next().unwrap_or(handle);
    paths.contains(&key(repo, file)) || paths.iter().any(|k| k.ends_with(&format!("\0{file}")))
}

fn dir_of(path: &str) -> String {
    match path.rfind('/') {
        Some(i) => path[..i].to_string(),
        None => String::new(),
    }
}

fn leaf(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// Is `path` inside `base`? An empty base is the repository root, which
/// everything is inside.
fn under(path: &str, base: &str) -> bool {
    base.is_empty() || path == base || path.starts_with(&format!("{base}/"))
}

/// The first path component below `base` — `api/src/routes.ts` under `api`
/// gives `api/src`. A file sitting directly in `base` is its own group, so a
/// top-level `main.rs` does not vanish.
fn top_dir(path: &str, base: &str) -> String {
    let rest = if base.is_empty() {
        path
    } else {
        path.strip_prefix(&format!("{base}/")).unwrap_or(path)
    };
    match rest.find('/') {
        Some(i) => {
            let seg = &rest[..i];
            if base.is_empty() {
                seg.to_string()
            } else {
                format!("{base}/{seg}")
            }
        }
        None => dir_of(path),
    }
}

fn dir_id(repo: &str, dir: &str) -> String {
    format!("dir:{repo}:{dir}")
}

fn loose_id(repo: &str) -> String {
    format!("repo:{repo}")
}

#[allow(clippy::too_many_arguments)]
fn blank_node(
    id: &str,
    kind: ArchKind,
    name: &str,
    path: Option<&str>,
    repo_id: &str,
    parent: Option<&str>,
) -> ArchNode {
    ArchNode {
        id: id.to_string(),
        kind,
        name: if name.is_empty() {
            "(root)".to_string()
        } else {
            name.to_string()
        },
        path: path.map(|p| p.to_string()),
        repo_id: repo_id.to_string(),
        parent: parent.map(|p| p.to_string()),
        files: 0,
        symbols: 0,
        endpoints: 0,
        tests: 0,
        workers: Vec::new(),
        activity: Vec::new(),
        leases: 0,
        tasks: Vec::new(),
        goals: Vec::new(),
        review_pending: 0,
        changed_recently: 0,
    }
}

/// Dependency cycles between nodes, by depth-first search.
///
/// Reported rather than broken. A cycle between two services is a real fact
/// about the architecture; a layout that quietly dropped an edge to stay tidy
/// would be hiding the most interesting thing on the picture.
fn find_cycles(nodes: &[ArchNode], edges: &[ArchEdge]) -> Vec<Vec<String>> {
    let mut out: Vec<Vec<String>> = Vec::new();
    let mut adj: HashMap<&str, Vec<&str>> = HashMap::new();
    for e in edges.iter().filter(|e| e.kind == EdgeKind::Imports) {
        adj.entry(&e.from).or_default().push(&e.to);
    }

    let mut state: HashMap<&str, u8> = HashMap::new(); // 0 unseen, 1 on stack, 2 done
    let mut stack: Vec<String> = Vec::new();

    fn walk<'a>(
        at: &'a str,
        adj: &HashMap<&'a str, Vec<&'a str>>,
        state: &mut HashMap<&'a str, u8>,
        stack: &mut Vec<String>,
        out: &mut Vec<Vec<String>>,
    ) {
        state.insert(at, 1);
        stack.push(at.to_string());
        for next in adj.get(at).into_iter().flatten() {
            match state.get(next).copied().unwrap_or(0) {
                0 => walk(next, adj, state, stack, out),
                1 => {
                    // Found one: the cycle is the stack from `next` onward.
                    if let Some(i) = stack.iter().position(|s| s == next) {
                        out.push(stack[i..].to_vec());
                    }
                }
                _ => {}
            }
        }
        stack.pop();
        state.insert(at, 2);
    }

    for n in nodes {
        if state.get(n.id.as_str()).copied().unwrap_or(0) == 0 {
            walk(&n.id, &adj, &mut state, &mut stack, &mut out);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::activity::{kind as akind, NewActivity};
    use crate::ids::DEFAULT_REPO_ID;
    use crate::lease::AcquireOptions;

    /// Two crates, one importing the other, plus a table the caller queries.
    fn fixture() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let w = |rel: &str, body: &str| {
            let p = dir.path().join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, body).unwrap();
        };
        w("api/package.json", r#"{"name":"api","version":"1.0.0"}"#);
        w(
            "api/src/routes.ts",
            "import { record } from '../../lib/src/ledger';\n\
             export function registerRoutes(app: any) {\n\
               app.post('/payments', createPayment);\n\
             }\n\
             export function createPayment(req: any) {\n\
               const rows = db.query('SELECT * FROM payments');\n\
               return record(1);\n\
             }\n",
        );
        w("lib/package.json", r#"{"name":"lib","version":"1.0.0"}"#);
        w(
            "lib/src/ledger.ts",
            "export function record(x: number) { return x; }\n",
        );
        w(
            "db/schema.sql",
            "CREATE TABLE payments (id INTEGER PRIMARY KEY);\n",
        );

        let mut store = Store::init(dir.path()).unwrap();
        crate::scan::scan(&mut store, DEFAULT_REPO_ID, dir.path(), &[], false).unwrap();
        store.register_agent("alice", "cursor").unwrap();
        store.register_agent("bob", "claude-code").unwrap();
        (dir, store)
    }

    fn node<'a>(g: &'a ArchGraph, name: &str) -> &'a ArchNode {
        g.nodes
            .iter()
            .find(|n| n.name == name)
            .unwrap_or_else(|| panic!("no node named {name} in {:?}", names(g)))
    }

    fn names(g: &ArchGraph) -> Vec<&str> {
        g.nodes.iter().map(|n| n.name.as_str()).collect()
    }

    fn ids(refs: &[ArchRef]) -> Vec<String> {
        refs.iter().map(|r| r.id.clone()).collect()
    }

    #[test]
    fn services_from_manifests_become_the_top_level_boxes() {
        let (_d, store) = fixture();
        let g = store.architecture(None, 1).unwrap();

        assert!(names(&g).contains(&"api"), "{:?}", names(&g));
        assert!(names(&g).contains(&"lib"));
        assert_eq!(node(&g, "api").kind, ArchKind::Service);
        assert!(
            node(&g, "api").files >= 1,
            "a service reports what its files add up to"
        );
    }

    #[test]
    fn an_import_between_two_services_is_one_dependency_edge() {
        let (_d, store) = fixture();
        let g = store.architecture(None, 1).unwrap();
        let api = &node(&g, "api").id;
        let lib = &node(&g, "lib").id;

        let deps: Vec<&ArchEdge> = g
            .edges
            .iter()
            .filter(|e| e.kind == EdgeKind::Imports)
            .collect();
        assert_eq!(deps.len(), 1, "one arrow, not one per import statement");
        assert_eq!(&deps[0].from, api, "the importer points at the imported");
        assert_eq!(&deps[0].to, lib);
    }

    #[test]
    fn a_table_is_its_own_box_with_an_arrow_into_it() {
        let (_d, store) = fixture();
        let g = store.architecture(None, 1).unwrap();

        let payments = node(&g, "payments");
        assert_eq!(payments.kind, ArchKind::Table);
        assert!(
            g.edges
                .iter()
                .any(|e| e.kind == EdgeKind::Queries && e.to == payments.id),
            "the database is a thing people point at: {:?}",
            g.edges
        );
    }

    #[test]
    fn depth_decides_how_far_the_collapse_goes() {
        let (_d, store) = fixture();
        let shallow = store.architecture(None, 1).unwrap();
        let deep = store.architecture(None, 3).unwrap();

        assert!(
            deep.nodes.len() > shallow.nodes.len(),
            "depth 3 shows files; depth 1 does not"
        );
        assert!(deep.nodes.iter().any(|n| n.kind == ArchKind::File));
        assert!(!shallow.nodes.iter().any(|n| n.kind == ArchKind::File));
        assert_eq!(store.architecture(None, 99).unwrap().depth, 3, "clamped");
    }

    #[test]
    fn a_worker_shows_up_on_the_box_containing_what_they_hold() {
        let (_d, mut store) = fixture();
        store
            .acquire_ref("record", "alice", &AcquireOptions::default())
            .unwrap();

        let g = store.architecture(None, 1).unwrap();
        assert_eq!(
            node(&g, "lib").workers,
            vec!["alice".to_string()],
            "holding a function inside lib puts you on lib"
        );
        assert_eq!(node(&g, "lib").leases, 1);
        assert!(
            node(&g, "api").workers.is_empty(),
            "and not on the service next door"
        );
    }

    #[test]
    fn live_editing_shows_up_on_the_box_too() {
        let (_d, mut store) = fixture();
        store
            .record_activity(&NewActivity::new(
                "bob",
                DEFAULT_REPO_ID,
                "api/src/routes.ts",
                akind::EDITING,
            ))
            .unwrap();

        let g = store.architecture(None, 1).unwrap();
        let api = node(&g, "api");
        assert_eq!(api.workers, vec!["bob".to_string()]);
        assert_eq!(api.activity.len(), 1);
        assert_eq!(api.activity[0].activity.kind, akind::EDITING);
    }

    #[test]
    fn a_task_scoped_into_a_service_lands_on_it_with_its_goal() {
        let (_d, mut store) = fixture();
        let goal = store.add_goal("ship refunds", 9, None, None).unwrap();
        let task = store
            .goal_decompose(&goal.id, "wire it", 9, &[], &["createPayment".to_string()])
            .unwrap();

        let g = store.architecture(None, 1).unwrap();
        let api = node(&g, "api");
        assert!(api.tasks.contains(&task.id));
        assert!(
            api.goals.contains(&goal.id),
            "a human reads goals, not task ids"
        );
    }

    #[test]
    fn node_detail_answers_the_questions_the_panel_asks() {
        let (_d, mut store) = fixture();
        store
            .acquire_ref("createPayment", "alice", &AcquireOptions::default())
            .unwrap();

        let g = store.architecture(None, 1).unwrap();
        let api_id = node(&g, "api").id.clone();
        let lib_id = node(&g, "lib").id.clone();
        let detail = store.arch_node(&api_id, 1).unwrap();

        assert_eq!(ids(&detail.depends_on), vec![lib_id]);
        assert_eq!(detail.depends_on[0].name, "lib", "an arrow has to say what it points at");
        assert_eq!(detail.held.len(), 1, "who is in here right now");
        assert_eq!(detail.held[0].agent, "alice");
        assert!(!detail.tables.is_empty(), "what it touches in the database");
        assert!(
            detail.routes.iter().any(|r| r.name == "createPayment"),
            "and what it exposes"
        );
        assert!(!detail.symbols.is_empty());
    }

    #[test]
    fn the_other_direction_of_a_dependency_is_reported_too() {
        let (_d, store) = fixture();
        let g = store.architecture(None, 1).unwrap();
        let lib = store.arch_node(&node(&g, "lib").id, 1).unwrap();

        assert_eq!(
            ids(&lib.depended_on_by),
            vec![node(&g, "api").id.clone()],
            "'who would I break' is the question that matters before a change"
        );
        assert!(lib.depends_on.is_empty());
    }

    #[test]
    fn an_unknown_node_is_an_error_rather_than_an_empty_panel() {
        let (_d, store) = fixture();
        match store.arch_node("dir:R1:nowhere", 2) {
            Ok(_) => panic!("expected an error naming the missing node"),
            Err(e) => assert!(e.to_string().contains("no such node")),
        }
    }

    #[test]
    fn a_repo_with_no_manifest_still_renders() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("src/pay.ts"),
            "export function charge(x: number) { return x; }\n",
        )
        .unwrap();
        let mut store = Store::init(dir.path()).unwrap();
        crate::scan::scan(&mut store, DEFAULT_REPO_ID, dir.path(), &[], false).unwrap();

        let g = store.architecture(None, 1).unwrap();
        assert!(
            !g.nodes.is_empty(),
            "files with no manifest above them still have to live somewhere"
        );
        assert!(g.nodes.iter().any(|n| n.files > 0));
    }

    #[test]
    fn a_dependency_cycle_is_reported_rather_than_hidden() {
        let (dir, mut store) = fixture();
        // Make lib import api back, closing the loop.
        std::fs::write(
            dir.path().join("lib/src/ledger.ts"),
            "import { createPayment } from '../../api/src/routes';\n\
             export function record(x: number) { return createPayment ? x : 0; }\n",
        )
        .unwrap();
        crate::scan::scan(&mut store, DEFAULT_REPO_ID, dir.path(), &[], true).unwrap();

        let g = store.architecture(None, 1).unwrap();
        assert!(
            !g.cycles.is_empty(),
            "a cycle between two services is the most interesting thing on the picture"
        );
    }
}
