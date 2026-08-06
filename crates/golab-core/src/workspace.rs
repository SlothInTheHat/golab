//! Workspace-wide operations, across every registered repository.
//!
//! A workspace can hold more than one repo (`golab repo add`), and `path` is
//! not a global key — `repo_id` is part of it. So every path-taking operation
//! has to work out which repo owns each path before it touches the index, or
//! scanning one repo prunes another's symbols as collateral damage.
//!
//! These live here rather than in the CLI because the CLI is not the only
//! caller any more: the daemon and the MCP adapter answer the same questions
//! and were previously hardcoded to `DEFAULT_REPO_ID`, which silently gave a
//! multi-repo workspace wrong answers over HTTP while the CLI got right ones.

use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::check;
use crate::guard::GuardReport;
use crate::ids;
use crate::model::{CheckReport, Repo, SymbolChange};
use crate::notify;
use crate::scan;
use crate::store::Store;

/// The filesystem root to scan for one registered repo.
pub fn repo_root(workspace_root: &Path, repo: &Repo) -> PathBuf {
    if repo.root_path == "." {
        workspace_root.to_path_buf()
    } else {
        workspace_root.join(&repo.root_path)
    }
}

/// Which registered repo owns each given path, by longest `root_path` prefix
/// match — falling back to the default repo when nothing more specific claims
/// it (or when the path can't be made workspace-relative).
pub fn group_paths_by_repo(
    store: &Store,
    workspace_root: &Path,
    repos: &[Repo],
    paths: &[PathBuf],
) -> Result<Vec<(Repo, Vec<PathBuf>)>> {
    let default = repos.iter().find(|r| r.id == ids::DEFAULT_REPO_ID).cloned();
    let mut groups: Vec<(Repo, Vec<PathBuf>)> = Vec::new();
    for p in paths {
        let abs = absolutize(p)?;
        let owner = match scan::relative_path(workspace_root, &abs) {
            Some(rel) => store.repo_for_path(&rel)?.or_else(|| default.clone()),
            None => default.clone(),
        };
        let Some(owner) = owner else { continue };
        match groups.iter_mut().find(|(r, _)| r.id == owner.id) {
            Some((_, v)) => v.push(abs),
            None => groups.push((owner, vec![abs])),
        }
    }
    Ok(groups)
}

/// Every repo with no path filter, or just the repos owning `paths`.
fn groups_for(store: &Store, root: &Path, paths: &[PathBuf]) -> Result<Vec<(Repo, Vec<PathBuf>)>> {
    let repos = store.repos()?;
    if paths.is_empty() {
        Ok(repos.into_iter().map(|r| (r, Vec::new())).collect())
    } else {
        group_paths_by_repo(store, root, &repos, paths)
    }
}

/// Scan (and notify for) every registered repo, or just the ones that own the
/// given paths. A single-repo workspace (the common case) behaves exactly as
/// a single `scan_and_notify` call always has.
pub fn scan_workspace(
    store: &mut Store,
    root: &Path,
    paths: &[PathBuf],
    force: bool,
) -> Result<scan::ScanStats> {
    let mut total = scan::ScanStats::default();
    for (repo, repo_paths) in groups_for(store, root, paths)? {
        let stats =
            notify::scan_and_notify(store, &repo.id, &repo_root(root, &repo), &repo_paths, force)?;
        accumulate_scan_stats(&mut total, &stats);
    }
    Ok(total)
}

pub fn accumulate_scan_stats(total: &mut scan::ScanStats, s: &scan::ScanStats) {
    total.files_seen += s.files_seen;
    total.files_indexed += s.files_indexed;
    total.files_unchanged += s.files_unchanged;
    total.files_removed += s.files_removed;
    total.files_failed += s.files_failed;
    total.symbols += s.symbols;
    total.edges += s.edges;
    total.services += s.services;
    total.tables += s.tables;
    total.endpoints += s.endpoints;
    total.tests += s.tests;
    total.leases_dropped += s.leases_dropped;
    total.elapsed_ms += s.elapsed_ms;
}

/// `golab check`, repo by repo, merged into one report.
pub fn check_workspace(
    store: &Store,
    root: &Path,
    agent: &str,
    paths: &[PathBuf],
) -> Result<CheckReport> {
    let mut merged = CheckReport {
        agent: agent.to_string(),
        changes: Vec::new(),
        violations: Vec::new(),
        covered: Vec::new(),
        unparsed: Vec::new(),
    };
    for (repo, repo_paths) in groups_for(store, root, paths)? {
        let report = check::check(store, &repo.id, &repo_root(root, &repo), agent, &repo_paths)?;
        merged.changes.extend(report.changes);
        merged.violations.extend(report.violations);
        merged.covered.extend(report.covered);
        merged.unparsed.extend(report.unparsed);
    }
    Ok(merged)
}

/// `golab diff`, repo by repo, merged into one change list.
pub fn diff_workspace(
    store: &Store,
    root: &Path,
    paths: &[PathBuf],
) -> Result<(Vec<SymbolChange>, Vec<String>)> {
    let mut changes = Vec::new();
    let mut unparsed = Vec::new();
    for (repo, repo_paths) in groups_for(store, root, paths)? {
        let (c, u) = check::changes(store, &repo.id, &repo_root(root, &repo), &repo_paths)?;
        changes.extend(c);
        unparsed.extend(u);
    }
    Ok((changes, unparsed))
}

/// Which repo owns a path, and what the path is called inside it.
///
/// The bridge every path-taking caller needs: editors hand out absolute
/// filesystem paths, and the index is keyed on `(repo_id, repo-relative
/// path)`. Never fails on an unknown path — it answers with the default repo
/// and a best-effort relative name, because the callers are on the critical
/// path of a keystroke and "I don't know where that is" has to mean "carry
/// on", never "your edit failed".
pub fn locate(store: &Store, root: &Path, path: &Path) -> Result<(String, String)> {
    let repos = store.repos()?;
    let abs = absolutize(path)?;
    let rel_to_workspace = scan::relative_path(root, &abs);

    let owner = match &rel_to_workspace {
        Some(rel) => store.repo_for_path(rel)?,
        None => None,
    };
    let owner = owner
        .or_else(|| repos.iter().find(|r| r.id == ids::DEFAULT_REPO_ID).cloned())
        .or_else(|| repos.first().cloned());

    Ok(match owner {
        Some(repo) => {
            let base = repo_root(root, &repo);
            let rel = scan::relative_path(&base, &abs)
                .or_else(|| rel_to_workspace.clone())
                .unwrap_or_else(|| abs.to_string_lossy().replace('\\', "/"));
            (repo.id, rel)
        }
        None => (
            ids::DEFAULT_REPO_ID.to_string(),
            rel_to_workspace.unwrap_or_else(|| abs.to_string_lossy().replace('\\', "/")),
        ),
    })
}

/// `Store::guard_edit` for a path anywhere in the workspace.
///
/// Unlike scan and check, a guard is about exactly one path, so it resolves
/// the owning repo and asks once.
pub fn guard_workspace(
    store: &Store,
    root: &Path,
    agent: &str,
    path: &Path,
    symbol: Option<&str>,
) -> Result<GuardReport> {
    let (repo_id, rel) = locate(store, root, path)?;
    store.guard_edit(&repo_id, agent, &rel, symbol)
}

fn absolutize(p: &Path) -> Result<PathBuf> {
    Ok(if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir()?.join(p)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::guard::GuardVerdict;
    use crate::lease::AcquireOptions;

    fn fixture() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("src/pay.ts"),
            "export function charge(x: number) { return x; }\n",
        )
        .unwrap();
        let mut store = Store::init(dir.path()).unwrap();
        scan_workspace(&mut store, dir.path(), &[], false).unwrap();
        store.register_agent("alice", "claude-code").unwrap();
        store.register_agent("bob", "cursor").unwrap();
        (dir, store)
    }

    #[test]
    fn guarding_an_absolute_path_finds_the_right_repo_relative_symbol() {
        let (dir, mut store) = fixture();
        store
            .acquire_ref("charge", "alice", &AcquireOptions::default())
            .unwrap();

        // An editor hook hands us an absolute path; the index is keyed on a
        // repo-relative one, and the guard has to bridge that itself.
        let abs = dir.path().join("src").join("pay.ts");
        let r = guard_workspace(&store, dir.path(), "bob", &abs, None).unwrap();
        assert_eq!(r.verdict, GuardVerdict::Denied);
        assert_eq!(r.path, "src/pay.ts");
        assert_eq!(r.conflicts[0].holder, "alice");
    }

    #[test]
    fn a_path_outside_the_workspace_is_allowed_rather_than_an_error() {
        let (dir, store) = fixture();
        let outside = std::env::temp_dir().join("nowhere-near-here.ts");
        let r = guard_workspace(&store, dir.path(), "bob", &outside, None).unwrap();
        assert_eq!(
            r.verdict,
            GuardVerdict::Allowed,
            "a hook on the critical path of a keystroke must never fail the edit"
        );
        assert!(r.unindexed);
    }

    #[test]
    fn a_second_repo_gets_its_own_answer_for_an_identical_relative_path() {
        let (dir, mut store) = fixture();
        // Two repos, both containing `src/pay.ts`. `path` alone cannot tell
        // them apart — only `repo_id` can.
        let other = dir.path().join("vendor");
        std::fs::create_dir_all(other.join("src")).unwrap();
        std::fs::write(
            other.join("src/pay.ts"),
            "export function charge(x: number) { return x + 1; }\n",
        )
        .unwrap();
        store.add_repo("vendor", Some("vendor")).unwrap();
        scan_workspace(&mut store, dir.path(), &[], false).unwrap();

        let main_file = dir.path().join("src").join("pay.ts");
        let vendor_file = other.join("src").join("pay.ts");
        let main_report = guard_workspace(&store, dir.path(), "bob", &main_file, None).unwrap();
        let vendor_report = guard_workspace(&store, dir.path(), "bob", &vendor_file, None).unwrap();

        assert_ne!(
            main_report.anchor, vendor_report.anchor,
            "identical relative paths in two repos are two different symbols"
        );
        assert_ne!(main_report.repo_id, vendor_report.repo_id);
    }
}
