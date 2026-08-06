//! Enforcement.
//!
//! Leasing is only a convention until something refuses the edit. `check`
//! re-parses the working tree, diffs it against the indexed snapshot at symbol
//! granularity, and reports every change the agent does not hold a lease for.
//! Wire it into a pre-commit hook (`atlas hook install`) and the runtime's
//! central promise stops being advisory.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::ids;
use crate::lang;
use crate::lease::conflict_from;
use crate::model::*;
use crate::parse;
use crate::scan;
use crate::store::Store;

/// Diff the working tree against the index, symbol by symbol, for one repo.
///
/// With `paths` empty, every indexed file plus every source file in the
/// workspace is considered, so new and deleted files are caught too. Scoped
/// to `repo_id`: without this, a workspace with more than one repo would try
/// to read every other repo's files off `root` and report them all deleted.
pub fn changes(store: &Store, repo_id: &str, root: &Path, paths: &[PathBuf]) -> Result<(Vec<SymbolChange>, Vec<String>)> {
    let mut targets: Vec<String> = Vec::new();
    if paths.is_empty() {
        targets.extend(store.indexed_files(repo_id)?);
        targets.extend(scan::collect_files(root, root)?);
    } else {
        for p in paths {
            let abs = if p.is_absolute() { p.clone() } else { root.join(p) };
            if abs.is_dir() {
                targets.extend(scan::collect_files(root, &abs)?);
            } else if let Some(rel) = scan::relative(root, &abs) {
                targets.push(rel);
            } else {
                targets.push(p.to_string_lossy().replace('\\', "/"));
            }
        }
    }
    targets.sort();
    targets.dedup();

    let mut out = Vec::new();
    let mut unparsed = Vec::new();
    for path in targets {
        if lang::for_path(&path).is_none() {
            continue;
        }
        match diff_file(store, repo_id, root, &path) {
            Ok(mut changes) => out.append(&mut changes),
            Err(_) => unparsed.push(path),
        }
    }
    Ok((out, unparsed))
}

fn diff_file(store: &Store, repo_id: &str, root: &Path, path: &str) -> Result<Vec<SymbolChange>> {
    let stored = store.symbols_in_file(repo_id, path)?;
    let stored_by_id: HashMap<&str, &Symbol> = stored.iter().map(|s| (s.id.as_str(), s)).collect();
    let file_id = ids::file_symbol_id(repo_id, path);

    // Unreadable means deleted, as far as the diff is concerned.
    let Some(bytes) = std::fs::read(root.join(path)).ok() else {
        return Ok(stored
            .iter()
            .map(|s| SymbolChange {
                handle: s.handle(),
                path: s.path.clone(),
                fqn: s.fqn.clone(),
                kind: s.kind,
                change: ChangeKind::Removed,
                symbol_id: Some(s.id.clone()),
                covering_id: s.parent_id.clone(),
            })
            .collect());
    };

    let pf = parse::parse_path(path, &bytes)?;
    let parsed_ids: Vec<String> = pf
        .symbols
        .iter()
        .map(|s| ids::symbol_id(repo_id, path, s.kind, &s.fqn, s.disambiguator))
        .collect();
    let parsed_set: HashSet<&str> = parsed_ids.iter().map(|s| s.as_str()).collect();

    let mut out = Vec::new();

    // The file itself: imports and top-level wiring.
    match stored_by_id.get(file_id.as_str()) {
        Some(f) if f.own_hash != pf.own_hash => out.push(SymbolChange {
            handle: path.to_string(),
            path: path.to_string(),
            fqn: path.to_string(),
            kind: SymbolKind::File,
            change: ChangeKind::Modified,
            symbol_id: Some(file_id.clone()),
            covering_id: None,
        }),
        None => out.push(SymbolChange {
            handle: path.to_string(),
            path: path.to_string(),
            fqn: path.to_string(),
            kind: SymbolKind::File,
            change: ChangeKind::Added,
            symbol_id: None,
            covering_id: None,
        }),
        _ => {}
    }

    for (i, s) in pf.symbols.iter().enumerate() {
        let id = &parsed_ids[i];
        match stored_by_id.get(id.as_str()) {
            Some(prev) => {
                if prev.own_hash != s.own_hash {
                    out.push(SymbolChange {
                        handle: prev.handle(),
                        path: path.to_string(),
                        fqn: s.fqn.clone(),
                        kind: s.kind,
                        change: ChangeKind::Modified,
                        symbol_id: Some(id.clone()),
                        covering_id: prev.parent_id.clone(),
                    });
                }
            }
            None => {
                // A new symbol is owned by the nearest scope that already
                // exists, so adding a method inside a leased class is legal.
                let mut covering = None;
                let mut cursor = s.parent;
                while let Some(p) = cursor {
                    let pid = &parsed_ids[p];
                    if stored_by_id.contains_key(pid.as_str()) {
                        covering = Some(pid.clone());
                        break;
                    }
                    cursor = pf.symbols[p].parent;
                }
                if covering.is_none() && stored_by_id.contains_key(file_id.as_str()) {
                    covering = Some(file_id.clone());
                }
                out.push(SymbolChange {
                    handle: format!("{path}:{}", s.fqn),
                    path: path.to_string(),
                    fqn: s.fqn.clone(),
                    kind: s.kind,
                    change: ChangeKind::Added,
                    symbol_id: None,
                    covering_id: covering,
                });
            }
        }
    }

    for s in &stored {
        if s.kind == SymbolKind::File || parsed_set.contains(s.id.as_str()) {
            continue;
        }
        out.push(SymbolChange {
            handle: s.handle(),
            path: path.to_string(),
            fqn: s.fqn.clone(),
            kind: s.kind,
            change: ChangeKind::Removed,
            symbol_id: Some(s.id.clone()),
            covering_id: s.parent_id.clone(),
        });
    }

    Ok(out)
}

/// Verify that every working-tree change is covered by a lease held by `agent`.
pub fn check(store: &Store, repo_id: &str, root: &Path, agent: &str, paths: &[PathBuf]) -> Result<CheckReport> {
    let (changes, unparsed) = changes(store, repo_id, root, paths)?;

    let held: HashMap<String, Lease> = store
        .active_leases(Some(agent))?
        .into_iter()
        .map(|l| (l.symbol_id.clone(), l))
        .collect();

    let mut violations = Vec::new();
    let mut covered = Vec::new();

    for change in changes {
        // The symbol itself for edits and deletions; the enclosing scope for
        // additions, which have no id of their own yet.
        let anchor = change
            .symbol_id
            .clone()
            .or_else(|| change.covering_id.clone());

        let Some(anchor) = anchor else {
            // Nothing in the index could possibly be leased (new file).
            covered.push(CoveredChange {
                change,
                lease_id: None,
                via: "unindexed (nothing to lease yet)".to_string(),
            });
            continue;
        };

        let chain = store.ancestors(&anchor)?;
        let mine = chain.iter().find_map(|s| held.get(&s.id).map(|l| (s, l)));
        match mine {
            Some((sym, lease)) => covered.push(CoveredChange {
                change,
                lease_id: Some(lease.id.clone()),
                via: sym.handle(),
            }),
            None => {
                // Not ours. Is it anyone's? That distinction is the difference
                // between "you forgot to lease" and "back off, agent 4 owns it".
                let held_by = chain
                    .iter()
                    .find_map(|s| {
                        store
                            .active_leases(None)
                            .ok()?
                            .into_iter()
                            .find(|l| l.symbol_id == s.id)
                            .map(|l| {
                                let relation = if s.id == anchor {
                                    ConflictRelation::Same
                                } else {
                                    ConflictRelation::Ancestor
                                };
                                conflict_from(&l, relation)
                            })
                    });
                violations.push(Violation { change, held_by });
            }
        }
    }

    Ok(CheckReport {
        agent: agent.to_string(),
        changes: violations
            .iter()
            .map(|v| v.change.clone())
            .chain(covered.iter().map(|c| c.change.clone()))
            .collect(),
        violations,
        covered,
        unparsed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lease::AcquireOptions;

    fn fixture() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("src/pay.py"),
            "import os\n\n\
             class PaymentService:\n\
             \x20   def process(self, id):\n\
             \x20       return 1\n\n\
             \x20   def refund(self, id):\n\
             \x20       return 2\n\n\
             def audit():\n\
             \x20   return 3\n",
        )
        .unwrap();
        let mut store = Store::init(dir.path()).unwrap();
        scan::scan(&mut store, ids::DEFAULT_REPO_ID, dir.path(), &[], false).unwrap();
        (dir, store)
    }

    fn write(dir: &tempfile::TempDir, body: &str) {
        std::fs::write(dir.path().join("src/pay.py"), body).unwrap();
    }

    #[test]
    fn clean_tree_has_no_changes() {
        let (dir, store) = fixture();
        let report = check(&store, ids::DEFAULT_REPO_ID, dir.path(), "agent-1", &[]).unwrap();
        assert!(report.ok());
        assert!(report.changes.is_empty(), "{:?}", report.changes);
    }

    #[test]
    fn editing_without_a_lease_is_a_violation() {
        let (dir, store) = fixture();
        write(
            &dir,
            "import os\n\nclass PaymentService:\n    def process(self, id):\n        return 99\n\n    def refund(self, id):\n        return 2\n\ndef audit():\n    return 3\n",
        );
        let report = check(&store, ids::DEFAULT_REPO_ID, dir.path(), "agent-1", &[]).unwrap();
        assert_eq!(report.violations.len(), 1, "{:?}", report.violations);
        let v = &report.violations[0];
        assert_eq!(v.change.fqn, "PaymentService.process");
        assert_eq!(v.change.change, ChangeKind::Modified);
        assert!(v.held_by.is_none());
    }

    #[test]
    fn editing_with_a_lease_passes_and_the_class_is_untouched() {
        let (dir, mut store) = fixture();
        let sym = store.resolve("PaymentService.process").unwrap();
        store
            .acquire(&sym.id, "agent-1", &AcquireOptions::default())
            .unwrap();
        write(
            &dir,
            "import os\n\nclass PaymentService:\n    def process(self, id):\n        return 99\n\n    def refund(self, id):\n        return 2\n\ndef audit():\n    return 3\n",
        );
        let report = check(&store, ids::DEFAULT_REPO_ID, dir.path(), "agent-1", &[]).unwrap();
        assert!(report.ok(), "{:?}", report.violations);
        assert_eq!(report.covered.len(), 1);
        assert_eq!(report.covered[0].via, "src/pay.py:PaymentService.process");
    }

    #[test]
    fn a_lease_on_the_class_covers_its_methods() {
        let (dir, mut store) = fixture();
        let sym = store.resolve("PaymentService").unwrap();
        store
            .acquire(&sym.id, "agent-1", &AcquireOptions::default())
            .unwrap();
        write(
            &dir,
            "import os\n\nclass PaymentService:\n    def process(self, id):\n        return 99\n\n    def refund(self, id):\n        return 2\n\ndef audit():\n    return 3\n",
        );
        let report = check(&store, ids::DEFAULT_REPO_ID, dir.path(), "agent-1", &[]).unwrap();
        assert!(report.ok(), "{:?}", report.violations);
        assert_eq!(report.covered[0].via, "src/pay.py:PaymentService");
    }

    #[test]
    fn violations_name_the_agent_that_owns_the_symbol() {
        let (dir, mut store) = fixture();
        let sym = store.resolve("PaymentService.process").unwrap();
        store
            .acquire(&sym.id, "agent-1", &AcquireOptions::default())
            .unwrap();
        write(
            &dir,
            "import os\n\nclass PaymentService:\n    def process(self, id):\n        return 99\n\n    def refund(self, id):\n        return 2\n\ndef audit():\n    return 3\n",
        );
        let report = check(&store, ids::DEFAULT_REPO_ID, dir.path(), "agent-2", &[]).unwrap();
        assert_eq!(report.violations.len(), 1);
        let held = report.violations[0].held_by.as_ref().unwrap();
        assert_eq!(held.holder, "agent-1");
        assert!(held.seconds_until_free > 0);
    }

    #[test]
    fn added_and_removed_symbols_are_attributed_to_their_scope() {
        let (dir, store) = fixture();
        write(
            &dir,
            "import os\n\nclass PaymentService:\n    def process(self, id):\n        return 1\n\n    def chargeback(self, id):\n        return 7\n\ndef audit():\n    return 3\n",
        );
        let report = check(&store, ids::DEFAULT_REPO_ID, dir.path(), "nobody", &[]).unwrap();
        let kinds: Vec<(&str, &str)> = report
            .violations
            .iter()
            .map(|v| (v.change.fqn.as_str(), v.change.change.as_str()))
            .collect();
        assert!(
            kinds.contains(&("PaymentService.chargeback", "added")),
            "{kinds:?}"
        );
        assert!(
            kinds.contains(&("PaymentService.refund", "removed")),
            "{kinds:?}"
        );

        // Both belong to the class, so one lease on it covers both.
        let mut store = store;
        let class = store.resolve("PaymentService").unwrap();
        store
            .acquire(&class.id, "agent-1", &AcquireOptions::default())
            .unwrap();
        let report = check(&store, ids::DEFAULT_REPO_ID, dir.path(), "agent-1", &[]).unwrap();
        assert!(report.ok(), "{:?}", report.violations);
    }

    #[test]
    fn touching_imports_requires_the_file_lease() {
        let (dir, mut store) = fixture();
        write(
            &dir,
            "import sys\n\nclass PaymentService:\n    def process(self, id):\n        return 1\n\n    def refund(self, id):\n        return 2\n\ndef audit():\n    return 3\n",
        );
        let report = check(&store, ids::DEFAULT_REPO_ID, dir.path(), "agent-1", &[]).unwrap();
        assert_eq!(report.violations.len(), 1);
        assert_eq!(report.violations[0].change.kind, SymbolKind::File);

        let file = store.resolve("src/pay.py").unwrap();
        store
            .acquire(&file.id, "agent-1", &AcquireOptions::default())
            .unwrap();
        assert!(check(&store, ids::DEFAULT_REPO_ID, dir.path(), "agent-1", &[]).unwrap().ok());
    }

    #[test]
    fn brand_new_files_are_allowed() {
        let (dir, store) = fixture();
        std::fs::write(dir.path().join("src/new.py"), "def brand_new():\n    pass\n").unwrap();
        let report = check(&store, ids::DEFAULT_REPO_ID, dir.path(), "agent-1", &[]).unwrap();
        assert!(report.ok(), "{:?}", report.violations);
        assert!(report.covered.iter().any(|c| c.change.path == "src/new.py"));
    }

    #[test]
    fn an_expired_lease_stops_covering_edits() {
        let (dir, mut store) = fixture();
        let sym = store.resolve("audit").unwrap();
        store
            .acquire(
                &sym.id,
                "agent-1",
                &AcquireOptions {
                    ttl_secs: 0,
                    ..Default::default()
                },
            )
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        store.expire_due().unwrap();
        write(
            &dir,
            "import os\n\nclass PaymentService:\n    def process(self, id):\n        return 1\n\n    def refund(self, id):\n        return 2\n\ndef audit():\n    return 42\n",
        );
        let report = check(&store, ids::DEFAULT_REPO_ID, dir.path(), "agent-1", &[]).unwrap();
        assert_eq!(report.violations.len(), 1);
        assert_eq!(report.violations[0].change.fqn, "audit");
    }

    /// `changes()` with no paths reads every file `indexed_files(repo_id)`
    /// knows about, then tries to read each off `root`. Without the repo
    /// scope, repo B's files would be read against repo A's root, fail, and
    /// be reported as wholesale deletions.
    #[test]
    fn check_in_a_multirepo_workspace_does_not_false_positive_delete_other_repos() {
        let dir = tempfile::tempdir().unwrap();
        for repo in ["repoA", "repoB"] {
            std::fs::create_dir_all(dir.path().join(repo).join("src")).unwrap();
            std::fs::write(
                dir.path().join(repo).join("src/index.ts"),
                "export function entry() { return 1; }\n",
            )
            .unwrap();
        }
        let mut store = Store::init(dir.path()).unwrap();
        scan::scan(&mut store, "R1", &dir.path().join("repoA"), &[], false).unwrap();
        scan::scan(&mut store, "R2", &dir.path().join("repoB"), &[], false).unwrap();

        let (found, _) = changes(&store, "R1", &dir.path().join("repoA"), &[]).unwrap();
        assert!(
            found.iter().all(|c| c.change != ChangeKind::Removed),
            "repo A's diff must never report repo B's files as deleted: {found:?}"
        );
    }
}
