//! Repositories: the "workspace contains repositories" half of multi-repo
//! support.
//!
//! A workspace always has at least `R1`, registered automatically by
//! `Store::init` with `root_path = "."` — so a single-repo workspace's stored
//! data is unchanged from before this module existed; every symbol is just
//! tagged with a `repo_id` that happens to always be `"R1"`.
//!
//! Everything above the knowledge graph (leases, tasks, goals, the scheduler)
//! stays repo-agnostic by construction: it only ever references `symbol_id`,
//! and identity already encodes `repo_id` (see `ids::symbol_id`). This module
//! only has to answer "what repos exist" and "which repo owns this path" —
//! scanning, resolving and diffing each stay scoped to one repo at a time by
//! their callers.

use std::path::Path;

use anyhow::{anyhow, Result};
use rusqlite::{params, OptionalExtension, Row};
use serde_json::json;

use crate::ids::{self, DEFAULT_REPO_ID};
use crate::model::{Repo, Symbol};
use crate::store::{self, Store};

/// `sym.handle()`, qualified with its repo's name when more than one repo is
/// registered. A single-repo workspace (the common case) gets the plain
/// handle back unchanged — this only does anything once `golab repo add`
/// has made a handle ambiguous on its own.
pub fn qualified_handle(store: &Store, sym: &Symbol) -> anyhow::Result<String> {
    let plain = sym.handle();
    if store.repos()?.len() <= 1 {
        return Ok(plain);
    }
    let prefix = store
        .repo(&sym.repo_id)?
        .map(|r| r.name)
        .unwrap_or_else(|| sym.repo_id.clone());
    Ok(format!("{prefix}/{plain}"))
}

impl Store {
    /// Register a repository under this workspace. `root_path` must be
    /// relative to the directory containing `.golab/` — an absolute path
    /// would break the workspace for anyone else who checks it out somewhere
    /// else.
    pub fn add_repo(&mut self, root_path: &str, name: Option<&str>) -> Result<Repo> {
        if Path::new(root_path).is_absolute() {
            return Err(anyhow!(
                "repo root_path must be relative to the workspace root, not absolute: {root_path}"
            ));
        }
        let root_path = root_path.trim_end_matches('/').to_string();
        let root_path = if root_path.is_empty() { ".".to_string() } else { root_path };
        let name = name
            .map(|s| s.to_string())
            .unwrap_or_else(|| default_repo_name(&root_path));
        self.write(move |tx| {
            let n: i64 = tx.query_row("SELECT COUNT(*) FROM repos", [], |r| r.get(0))?;
            let id = format!("R{}", n + 1);
            let now = ids::now_ms();
            tx.execute(
                "INSERT INTO repos(id, name, root_path, added_at) VALUES (?1, ?2, ?3, ?4)",
                params![id, name, root_path, now],
            )?;
            store::emit(
                tx,
                "repo.added",
                None,
                None,
                None,
                json!({ "repo": id, "name": name, "root_path": root_path }),
            )?;
            Ok(Repo { id, name, root_path, added_at: now })
        })
    }

    /// Ensure `R1` exists, pointed at the workspace root. Called once from
    /// `Store::init`; idempotent, since `init` itself must be.
    pub(crate) fn ensure_default_repo(&mut self) -> Result<()> {
        let existing: i64 = self.conn().query_row("SELECT COUNT(*) FROM repos", [], |r| r.get(0))?;
        if existing > 0 {
            return Ok(());
        }
        self.write(|tx| {
            let now = ids::now_ms();
            tx.execute(
                "INSERT INTO repos(id, name, root_path, added_at) VALUES (?1, 'root', '.', ?2)",
                params![DEFAULT_REPO_ID, now],
            )?;
            Ok(())
        })
    }

    pub fn repos(&self) -> Result<Vec<Repo>> {
        let mut stmt = self
            .conn()
            .prepare("SELECT id, name, root_path, added_at FROM repos ORDER BY id")?;
        let rows = stmt.query_map([], row_to_repo)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    pub fn repo(&self, id: &str) -> Result<Option<Repo>> {
        Ok(self
            .conn()
            .query_row(
                "SELECT id, name, root_path, added_at FROM repos WHERE id = ?1",
                params![id],
                row_to_repo,
            )
            .optional()?)
    }

    /// Resolve an id or name to a repo id, for the `repo:path:Fqn` resolve
    /// tier. Not an error if nothing matches — the caller falls through to
    /// treating the whole query as an ordinary (non-repo-qualified) form.
    pub(crate) fn resolve_repo_id(&self, id_or_name: &str) -> Result<Option<String>> {
        Ok(self
            .conn()
            .query_row(
                "SELECT id FROM repos WHERE id = ?1 OR name = ?1",
                params![id_or_name],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// Which registered repo owns a workspace-relative path, by longest
    /// `root_path` prefix match. `"."` (R1's default) matches everything, so
    /// it's always the fallback unless a more specific repo claims the path.
    pub fn repo_for_path(&self, workspace_relative: &str) -> Result<Option<Repo>> {
        let repos = self.repos()?;
        Ok(repos
            .into_iter()
            .filter(|r| repo_contains(&r.root_path, workspace_relative))
            .max_by_key(|r| r.root_path.len()))
    }
}

fn repo_contains(root_path: &str, target: &str) -> bool {
    if root_path == "." {
        return true;
    }
    target == root_path || target.starts_with(&format!("{root_path}/"))
}

fn default_repo_name(root_path: &str) -> String {
    Path::new(root_path)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| root_path.to_string())
}

fn row_to_repo(r: &Row) -> rusqlite::Result<Repo> {
    Ok(Repo {
        id: r.get(0)?,
        name: r.get(1)?,
        root_path: r.get(2)?,
        added_at: r.get(3)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_registers_the_default_repo() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::init(dir.path()).unwrap();
        let repos = store.repos().unwrap();
        assert_eq!(repos.len(), 1);
        assert_eq!(repos[0].id, "R1");
        assert_eq!(repos[0].root_path, ".");
    }

    #[test]
    fn init_is_idempotent_about_the_default_repo() {
        let dir = tempfile::tempdir().unwrap();
        Store::init(dir.path()).unwrap();
        let store = Store::init(dir.path()).unwrap();
        assert_eq!(store.repos().unwrap().len(), 1, "must not register R1 twice");
    }

    #[test]
    fn adding_a_repo_assigns_the_next_id_and_a_default_name() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::init(dir.path()).unwrap();
        let r2 = store.add_repo("../frontend", None).unwrap();
        assert_eq!(r2.id, "R2");
        assert_eq!(r2.name, "frontend");
        assert_eq!(store.repos().unwrap().len(), 2);
    }

    #[test]
    fn an_absolute_root_path_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::init(dir.path()).unwrap();
        let absolute = if cfg!(windows) { r"C:\abs\path" } else { "/abs/path" };
        assert!(store.add_repo(absolute, None).is_err());
    }

    #[test]
    fn resolve_repo_id_matches_by_id_or_name() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::init(dir.path()).unwrap();
        store.add_repo("frontend", Some("web")).unwrap();
        assert_eq!(store.resolve_repo_id("R2").unwrap().as_deref(), Some("R2"));
        assert_eq!(store.resolve_repo_id("web").unwrap().as_deref(), Some("R2"));
        assert_eq!(store.resolve_repo_id("nope").unwrap(), None);
    }

    #[test]
    fn repo_for_path_picks_the_longest_matching_root() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::init(dir.path()).unwrap();
        store.add_repo("frontend", None).unwrap();
        store.add_repo("frontend/widgets", Some("widgets")).unwrap();

        assert_eq!(store.repo_for_path("frontend/src/app.tsx").unwrap().unwrap().id, "R2");
        assert_eq!(
            store.repo_for_path("frontend/widgets/button.tsx").unwrap().unwrap().id,
            "R3",
            "the more specific repo wins over its own parent"
        );
        assert_eq!(store.repo_for_path("backend/main.py").unwrap().unwrap().id, "R1");
    }
}
