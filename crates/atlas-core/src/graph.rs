//! Symbol graph queries.
//!
//! The index is not just a list of leasable names — it is a graph. That makes
//! it possible to answer the question an agent should ask *before* it starts
//! editing: if I change this, what else moves, and who owns that?

use std::collections::{HashMap, HashSet, VecDeque};

use anyhow::Result;
use rusqlite::params;
use serde::Serialize;

use crate::model::*;
use crate::store::{row_to_symbol, Store, QUALIFIED_COLS, SYMBOL_COLS};

#[derive(Debug, Clone, Serialize)]
pub struct Neighbor {
    pub symbol: Symbol,
    pub kind: EdgeKind,
}

#[derive(Debug, Clone, Serialize)]
pub struct Neighbors {
    pub symbol: Symbol,
    /// Symbols that reference this one.
    pub callers: Vec<Neighbor>,
    /// Symbols this one references.
    pub callees: Vec<Neighbor>,
    /// Nested symbols (methods of a class, symbols of a file).
    pub children: Vec<Symbol>,
}

/// A symbol reachable from a change, with who currently owns it.
#[derive(Debug, Clone, Serialize)]
pub struct ImpactNode {
    pub symbol: Symbol,
    /// Edge hops away from the origin.
    pub distance: usize,
    pub lease: Option<Lease>,
}

impl Store {
    pub fn neighbors(&self, id: &str) -> Result<Neighbors> {
        let symbol = self
            .symbol(id)?
            .ok_or_else(|| anyhow::anyhow!("unknown symbol: {id}"))?;

        let callers = self.edge_neighbors(id, Direction::In)?;
        let callees = self.edge_neighbors(id, Direction::Out)?;
        let children = self.children(id)?;
        Ok(Neighbors {
            symbol,
            callers,
            callees,
            children,
        })
    }

    pub fn children(&self, id: &str) -> Result<Vec<Symbol>> {
        let mut stmt = self.conn().prepare(&format!(
            "SELECT {SYMBOL_COLS} FROM symbols WHERE parent_id = ?1 ORDER BY start_byte"
        ))?;
        let rows = stmt.query_map(params![id], row_to_symbol)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    fn edge_neighbors(&self, id: &str, dir: Direction) -> Result<Vec<Neighbor>> {
        // `contains` is structure, not behaviour: it does not belong in
        // "what would my change reach".
        let sql = match dir {
            Direction::In => format!(
                "SELECT {QUALIFIED_COLS}, e.kind FROM edges e JOIN symbols s ON s.id = e.src \
                 WHERE e.dst = ?1 AND e.kind != 'contains'"
            ),
            Direction::Out => format!(
                "SELECT {QUALIFIED_COLS}, e.kind FROM edges e JOIN symbols s ON s.id = e.dst \
                 WHERE e.src = ?1 AND e.kind != 'contains'"
            ),
        };
        let mut stmt = self.conn().prepare(&sql)?;
        let rows = stmt.query_map(params![id], |r| {
            // QUALIFIED_COLS has 16 columns (0..=15); the joined e.kind lands
            // right after at 16. Bumping SYMBOL_COLS shifts this — see the
            // comment on QUALIFIED_COLS in store.rs.
            let kind: String = r.get(16)?;
            Ok(Neighbor {
                symbol: row_to_symbol(r)?,
                kind: EdgeKind::parse(&kind).unwrap_or(EdgeKind::Uses),
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Everything that could be disturbed by changing `id`, out to `depth`
    /// hops of callers, annotated with the lease holder if there is one.
    ///
    /// This is the raw material for predictive conflict detection: "your
    /// payments refactor reaches `authenticate()`, which agent-4 holds for
    /// another 2 minutes."
    pub fn impact(&self, id: &str, depth: usize) -> Result<Vec<ImpactNode>> {
        let leases: HashMap<String, Lease> = self
            .active_leases(None)?
            .into_iter()
            .map(|l| (l.symbol_id.clone(), l))
            .collect();

        let mut seen: HashSet<String> = HashSet::from([id.to_string()]);
        let mut queue: VecDeque<(String, usize)> = VecDeque::from([(id.to_string(), 0)]);
        let mut out = Vec::new();

        while let Some((cur, dist)) = queue.pop_front() {
            if dist >= depth {
                continue;
            }
            for n in self.edge_neighbors(&cur, Direction::In)? {
                if !seen.insert(n.symbol.id.clone()) {
                    continue;
                }
                queue.push_back((n.symbol.id.clone(), dist + 1));
                out.push(ImpactNode {
                    lease: leases.get(&n.symbol.id).cloned(),
                    symbol: n.symbol,
                    distance: dist + 1,
                });
            }
        }
        out.sort_by_key(|n| (n.distance, n.symbol.path.clone(), n.symbol.start_byte));
        Ok(out)
    }
}

enum Direction {
    In,
    Out,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lease::AcquireOptions;
    use crate::scan;

    fn fixture() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("app.py"),
            "def db_write(x):\n    return x\n\n\
             def save_user(u):\n    return db_write(u)\n\n\
             def signup(form):\n    return save_user(form)\n",
        )
        .unwrap();
        let mut store = Store::init(dir.path()).unwrap();
        scan::scan(&mut store, crate::ids::DEFAULT_REPO_ID, dir.path(), &[], false).unwrap();
        (dir, store)
    }

    #[test]
    fn callers_and_callees_resolve() {
        let (_d, store) = fixture();
        let save = store.resolve("save_user").unwrap();
        let n = store.neighbors(&save.id).unwrap();
        assert!(n.callers.iter().any(|c| c.symbol.name == "signup"), "{:?}", n.callers);
        assert!(n.callees.iter().any(|c| c.symbol.name == "db_write"), "{:?}", n.callees);
    }

    #[test]
    fn impact_walks_callers_transitively_and_names_lease_holders() {
        let (_d, mut store) = fixture();
        let signup = store.resolve("signup").unwrap();
        store
            .acquire(&signup.id, "agent-4", &AcquireOptions::default())
            .unwrap();

        let db = store.resolve("db_write").unwrap();
        let impact = store.impact(&db.id, 3).unwrap();
        let names: Vec<&str> = impact.iter().map(|n| n.symbol.name.as_str()).collect();
        assert!(names.contains(&"save_user"), "{names:?}");
        assert!(names.contains(&"signup"), "{names:?}");

        let hit = impact.iter().find(|n| n.symbol.name == "signup").unwrap();
        assert_eq!(hit.distance, 2);
        assert_eq!(hit.lease.as_ref().unwrap().agent, "agent-4");
    }

    #[test]
    fn impact_respects_depth() {
        let (_d, store) = fixture();
        let db = store.resolve("db_write").unwrap();
        let shallow = store.impact(&db.id, 1).unwrap();
        assert_eq!(shallow.len(), 1);
        assert_eq!(shallow[0].symbol.name, "save_user");
    }
}
