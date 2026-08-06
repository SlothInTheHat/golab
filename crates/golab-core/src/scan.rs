//! Repository scanner: keeps the knowledge graph in sync with the working tree.
//!
//! Scanning is incremental (files whose content hash is unchanged are skipped)
//! and lease-aware: if an indexed symbol disappears, any lease on it is
//! retired with an event rather than left dangling.
//!
//! The graph it maintains is more than symbols. A scan discovers:
//!
//! - **services**, from manifests, which become the parents of their files;
//! - **files and symbols**, from tree-sitter;
//! - **tables**, from SQL DDL;
//! - **roles** — which functions are HTTP handlers, which are tests;
//! - **edges** — contains, imports, calls, uses, queries and tests.
//!
//! Imports are resolved before calls on purpose: knowing that `pay.ts` imports
//! `./ledger` is what lets a call to `record()` resolve to the right one of
//! three same-named functions.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::{params, Transaction};
use serde_json::json;

use crate::imports::{self, FileIndex};
use crate::model::*;
use crate::parse::{self, ParsedFile};
use crate::roles;
use crate::sql;
use crate::store::{self, Store};
use crate::topology::{self, Service};
use crate::{ids, lang};

/// Files above this size are skipped: generated bundles and vendored blobs are
/// not things agents hand-edit, and parsing them dominates scan time.
const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;

#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct ScanStats {
    pub files_seen: usize,
    pub files_indexed: usize,
    pub files_unchanged: usize,
    pub files_removed: usize,
    pub files_failed: usize,
    pub symbols: usize,
    pub edges: usize,
    pub services: usize,
    pub tables: usize,
    pub endpoints: usize,
    pub tests: usize,
    pub leases_dropped: usize,
    pub elapsed_ms: u128,
}

/// Everything the scanner learned about one file, kept until edges are built.
struct Indexed {
    path: String,
    pf: ParsedFile,
    /// Symbol ids parallel to `pf.symbols`.
    sym_ids: Vec<String>,
    file_id: String,
    /// Roles parallel to `pf.symbols`.
    roles: Vec<Option<Role>>,
    source: Vec<u8>,
}

/// Index `root` into repo `repo_id`. When `paths` is empty the whole
/// workspace is walked and files that vanished are pruned; otherwise only the
/// given paths are refreshed. Every read and write here is scoped to
/// `repo_id` — a partial or full scan of one repo must never touch another
/// repo's rows, even when relative paths collide between them.
pub fn scan(store: &mut Store, repo_id: &str, root: &Path, paths: &[PathBuf], force: bool) -> Result<ScanStats> {
    let started = std::time::Instant::now();
    let mut stats = ScanStats::default();
    let full = paths.is_empty();

    let found = if full {
        collect(root, root)?
    } else {
        let mut acc = Found::default();
        for p in paths {
            let abs = if p.is_absolute() { p.clone() } else { root.join(p) };
            if abs.is_dir() {
                acc.merge(collect(root, &abs)?);
            } else if abs.is_file() {
                if let Some(rel) = relative(root, &abs) {
                    acc.push(rel);
                }
            }
        }
        acc
    };
    stats.files_seen = found.sources.len() + found.sql.len();

    // Services first: files hang off them, so they must exist before any file
    // symbol is written.
    let services = if full {
        let discovered: Vec<Service> = found
            .manifests
            .iter()
            .filter_map(|m| topology::read_manifest(root, m))
            .collect();
        store.write(|tx| write_services(tx, repo_id, &discovered))?;
        discovered
    } else {
        store.services(repo_id)?
    };
    stats.services = services.len();

    let mut indexed: Vec<Indexed> = Vec::new();

    // Pass 1a: SQL schemas become table nodes.
    for rel in &found.sql {
        let Ok(bytes) = std::fs::read(root.join(rel)) else {
            stats.files_failed += 1;
            continue;
        };
        let hash = ids::content_hash(&bytes);
        if !force && store.file_hash(repo_id, rel)?.as_deref() == Some(hash.as_str()) {
            stats.files_unchanged += 1;
            continue;
        }
        let defs = sql::tables(&bytes);
        let service = topology::service_for(rel, &services).map(|s| service_id(repo_id, s));
        let dropped = store.write(|tx| {
            write_sql_file(tx, repo_id, rel, &bytes, &hash, &defs, service.as_deref())
        })?;
        stats.leases_dropped += dropped;
        stats.files_indexed += 1;
        stats.tables += defs.len();
        stats.symbols += defs.len() + 1;
    }

    // Pass 1b: source files.
    for rel in &found.sources {
        let bytes = match std::fs::read(root.join(rel)) {
            Ok(b) => b,
            Err(_) => {
                stats.files_failed += 1;
                continue;
            }
        };
        let hash = ids::content_hash(&bytes);
        if !force && store.file_hash(repo_id, rel)?.as_deref() == Some(hash.as_str()) {
            stats.files_unchanged += 1;
            continue;
        }
        let pf = match parse::parse_path(rel, &bytes) {
            Ok(pf) => pf,
            Err(_) => {
                stats.files_failed += 1;
                continue;
            }
        };

        let (symbol_roles, file_role) = detect_roles(rel, &bytes, &pf);
        stats.endpoints += symbol_roles.iter().filter(|r| r.0 == Some(Role::Api)).count();
        stats.tests += symbol_roles.iter().filter(|r| r.0 == Some(Role::Test)).count();

        let service = topology::service_for(rel, &services).map(|s| service_id(repo_id, s));
        let (sym_ids, file_id, dropped) = store.write(|tx| {
            write_file_symbols(
                tx,
                repo_id,
                rel,
                &pf,
                &hash,
                bytes.len() as u64,
                &symbol_roles,
                file_role,
                service.as_deref(),
            )
        })?;

        stats.leases_dropped += dropped;
        stats.files_indexed += 1;
        stats.symbols += pf.symbols.len() + 1;
        indexed.push(Indexed {
            path: rel.clone(),
            roles: symbol_roles.iter().map(|r| r.0).collect(),
            pf,
            sym_ids,
            file_id,
            source: bytes,
        });
    }

    // Prune what disappeared. Scoped to this repo only — indexed_files(repo_id)
    // never returns another repo's files, so a full scan can never mistake
    // them for gone and delete them as collateral damage.
    if full {
        let present: HashSet<&String> = found.sources.iter().chain(found.sql.iter()).collect();
        for path in store.indexed_files(repo_id)? {
            if !present.contains(&path) {
                let dropped = store.write(|tx| remove_file(tx, repo_id, &path))?;
                stats.leases_dropped += dropped;
                stats.files_removed += 1;
            }
        }
        let live: HashSet<String> = services.iter().map(|s| s.manifest.clone()).collect();
        store.write(|tx| prune_services(tx, repo_id, &live))?;
    }

    // Pass 2: edges, once every node in the repository is known. Scoped to
    // this repo: edges must never be wired across repos just because two
    // repos happen to share a relative path (e.g. both have src/index.ts).
    if !indexed.is_empty() {
        let ctx = EdgeContext::build(store, repo_id)?;
        for f in &indexed {
            let edges = build_edges(f, repo_id, &ctx);
            let n = edges.len();
            store.write(|tx| {
                for sid in f.sym_ids.iter().chain(std::iter::once(&f.file_id)) {
                    tx.execute("DELETE FROM edges WHERE src = ?1", params![sid])?;
                }
                for e in &edges {
                    tx.execute(
                        "INSERT OR IGNORE INTO edges(src, dst, kind) VALUES (?1, ?2, ?3)",
                        params![e.src, e.dst, e.kind.as_str()],
                    )?;
                }
                Ok(())
            })?;
            stats.edges += n;
        }
        // Service-level dependencies are the aggregate of their files'.
        if !services.is_empty() {
            let rolled = store.write(roll_up_service_edges)?;
            stats.edges += rolled;
        }
    }

    stats.elapsed_ms = started.elapsed().as_millis();
    if stats.files_indexed > 0 || stats.files_removed > 0 {
        let summary = json!({
            "files_indexed": stats.files_indexed,
            "files_unchanged": stats.files_unchanged,
            "files_removed": stats.files_removed,
            "symbols": stats.symbols,
            "edges": stats.edges,
            "services": stats.services,
            "tables": stats.tables,
            "endpoints": stats.endpoints,
        });
        store.write(|tx| {
            store::emit(tx, "scan.completed", None, None, None, summary)?;
            Ok(())
        })?;
    }
    Ok(stats)
}

// ------------------------------------------------------------------- roles

/// Roles and metadata for each symbol in a file, plus the file's own role.
fn detect_roles(
    path: &str,
    source: &[u8],
    pf: &ParsedFile,
) -> (Vec<(Option<Role>, serde_json::Value)>, Option<Role>) {
    let mut out = vec![(None, json!({})); pf.symbols.len()];
    for hit in roles::tests(path, source, &pf.symbols) {
        out[hit.symbol] = (Some(Role::Test), hit.meta);
    }
    // Endpoints win over tests: a routed handler inside a test file is still
    // the interesting thing about it.
    let mut routes: HashMap<usize, Vec<serde_json::Value>> = HashMap::new();
    for hit in roles::endpoints(source, &pf.symbols) {
        routes.entry(hit.symbol).or_default().push(hit.meta);
    }
    for (i, mut metas) in routes {
        // One function can register several routes; keep the first as the
        // headline and list the rest, so none of them is silently lost.
        let first = metas.remove(0);
        let meta = if metas.is_empty() {
            first
        } else {
            let mut all = vec![first.clone()];
            all.extend(metas);
            json!({
                "method": first["method"],
                "path": first["path"],
                "routes": all,
            })
        };
        out[i] = (Some(Role::Api), meta);
    }
    let file_role = if roles::is_test_path(path) {
        Some(Role::Test)
    } else {
        None
    };
    (out, file_role)
}

// ------------------------------------------------------------------ writing

fn service_id(repo_id: &str, s: &Service) -> String {
    ids::symbol_id(repo_id, &s.manifest, SymbolKind::Service, &s.name, 0)
}

fn write_services(tx: &Transaction, repo_id: &str, services: &[Service]) -> Result<()> {
    let now = ids::now_ms();
    for s in services {
        let parent = topology::parent_service(s, services).map(|p| service_id(repo_id, p));
        tx.execute(
            "INSERT INTO symbols(id, path, lang, kind, name, fqn, parent_id, start_byte, \
                 end_byte, start_line, end_line, body_hash, own_hash, role, meta, \
                 disambiguator, updated_at, repo_id) \
             VALUES (?1, ?2, ?3, 'service', ?4, ?4, ?5, 0, 0, 0, 0, '', '', 'service', ?6, 0, ?7, ?8) \
             ON CONFLICT(id) DO UPDATE SET parent_id=excluded.parent_id, meta=excluded.meta, \
                 updated_at=excluded.updated_at",
            params![
                service_id(repo_id, s),
                s.manifest,
                s.ecosystem,
                s.name,
                parent,
                json!({ "ecosystem": s.ecosystem, "dir": s.dir }).to_string(),
                now,
                repo_id,
            ],
        )?;
    }
    Ok(())
}

fn prune_services(tx: &Transaction, repo_id: &str, live_manifests: &HashSet<String>) -> Result<()> {
    let mut stmt =
        tx.prepare("SELECT id, path FROM symbols WHERE kind = 'service' AND repo_id = ?1")?;
    let existing: Vec<(String, String)> = stmt
        .query_map(params![repo_id], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<Result<Vec<_>, _>>()?;
    drop(stmt);
    for (id, manifest) in existing {
        if !live_manifests.contains(&manifest) {
            retire_leases_on(tx, &id, &manifest, "service removed")?;
            tx.execute("DELETE FROM edges WHERE src = ?1 OR dst = ?1", params![id])?;
            tx.execute("UPDATE symbols SET parent_id = NULL WHERE parent_id = ?1", params![id])?;
            tx.execute("DELETE FROM symbols WHERE id = ?1", params![id])?;
        }
    }
    Ok(())
}

/// Write one file's symbols, replacing whatever was there before.
/// Returns (symbol ids in parse order, file symbol id, leases dropped).
#[allow(clippy::too_many_arguments)]
fn write_file_symbols(
    tx: &Transaction,
    repo_id: &str,
    path: &str,
    pf: &ParsedFile,
    content_hash: &str,
    size: u64,
    symbol_roles: &[(Option<Role>, serde_json::Value)],
    file_role: Option<Role>,
    service: Option<&str>,
) -> Result<(Vec<String>, String, usize)> {
    let now = ids::now_ms();
    let file_id = ids::file_symbol_id(repo_id, path);

    let mut keep: Vec<String> = vec![file_id.clone()];
    let mut sym_ids: Vec<String> = Vec::with_capacity(pf.symbols.len());
    for s in &pf.symbols {
        let id = ids::symbol_id(repo_id, path, s.kind, &s.fqn, s.disambiguator);
        sym_ids.push(id.clone());
        keep.push(id);
    }
    let dropped = retire_missing_symbols(tx, repo_id, path, &keep)?;

    upsert_symbol(
        tx,
        &SymbolRow {
            id: &file_id,
            repo_id,
            path,
            lang: pf.lang,
            kind: SymbolKind::File,
            name: path.rsplit('/').next().unwrap_or(path),
            fqn: path,
            parent: service,
            start_byte: 0,
            end_byte: size as i64,
            start_line: 0,
            end_line: pf.symbols.last().map(|s| s.end_line as i64).unwrap_or(0),
            body_hash: content_hash,
            own_hash: &pf.own_hash,
            role: file_role,
            meta: &json!({}),
            disambiguator: 0,
        },
        now,
    )?;

    for (i, s) in pf.symbols.iter().enumerate() {
        let parent_id = match s.parent {
            Some(p) => sym_ids[p].clone(),
            None => file_id.clone(),
        };
        let (role, meta) = &symbol_roles[i];
        upsert_symbol(
            tx,
            &SymbolRow {
                id: &sym_ids[i],
                repo_id,
                path,
                lang: pf.lang,
                kind: s.kind,
                name: &s.name,
                fqn: &s.fqn,
                parent: Some(&parent_id),
                start_byte: s.start_byte as i64,
                end_byte: s.end_byte as i64,
                start_line: s.start_line as i64,
                end_line: s.end_line as i64,
                body_hash: &s.body_hash,
                own_hash: &s.own_hash,
                role: *role,
                meta,
                disambiguator: s.disambiguator as i64,
            },
            now,
        )?;
    }

    upsert_file(tx, repo_id, path, pf.lang, content_hash, size, now)?;
    Ok((sym_ids, file_id, dropped))
}

/// Index a `.sql` file: the file itself, plus a node per table it declares.
#[allow(clippy::too_many_arguments)]
fn write_sql_file(
    tx: &Transaction,
    repo_id: &str,
    path: &str,
    source: &[u8],
    content_hash: &str,
    defs: &[sql::TableDef],
    service: Option<&str>,
) -> Result<usize> {
    let now = ids::now_ms();
    let file_id = ids::file_symbol_id(repo_id, path);
    let mut keep = vec![file_id.clone()];
    for d in defs {
        keep.push(ids::symbol_id(repo_id, path, SymbolKind::Table, &d.name, 0));
    }
    let dropped = retire_missing_symbols(tx, repo_id, path, &keep)?;

    upsert_symbol(
        tx,
        &SymbolRow {
            id: &file_id,
            repo_id,
            path,
            lang: "sql",
            kind: SymbolKind::File,
            name: path.rsplit('/').next().unwrap_or(path),
            fqn: path,
            parent: service,
            start_byte: 0,
            end_byte: source.len() as i64,
            start_line: 0,
            end_line: source.iter().filter(|b| **b == b'\n').count() as i64,
            body_hash: content_hash,
            own_hash: content_hash,
            role: Some(Role::Schema),
            meta: &json!({ "tables": defs.len() }),
            disambiguator: 0,
        },
        now,
    )?;

    for d in defs {
        let id = ids::symbol_id(repo_id, path, SymbolKind::Table, &d.name, 0);
        let text = source.get(d.start_byte..d.end_byte).unwrap_or(&[]);
        upsert_symbol(
            tx,
            &SymbolRow {
                id: &id,
                repo_id,
                path,
                lang: "sql",
                kind: SymbolKind::Table,
                name: &d.name,
                fqn: &d.name,
                parent: Some(&file_id),
                start_byte: d.start_byte as i64,
                end_byte: d.end_byte as i64,
                start_line: d.line as i64,
                end_line: d.line as i64,
                body_hash: &ids::content_hash(text),
                own_hash: &ids::content_hash(text),
                role: Some(Role::Schema),
                meta: &json!({ "statement": d.statement }),
                disambiguator: 0,
            },
            now,
        )?;
        tx.execute(
            "INSERT OR IGNORE INTO edges(src, dst, kind) VALUES (?1, ?2, 'contains')",
            params![file_id, id],
        )?;
    }

    upsert_file(tx, repo_id, path, "sql", content_hash, source.len() as u64, now)?;
    Ok(dropped)
}

struct SymbolRow<'a> {
    id: &'a str,
    repo_id: &'a str,
    path: &'a str,
    lang: &'a str,
    kind: SymbolKind,
    name: &'a str,
    fqn: &'a str,
    parent: Option<&'a str>,
    start_byte: i64,
    end_byte: i64,
    start_line: i64,
    end_line: i64,
    body_hash: &'a str,
    own_hash: &'a str,
    role: Option<Role>,
    meta: &'a serde_json::Value,
    disambiguator: i64,
}

fn upsert_symbol(tx: &Transaction, row: &SymbolRow, now: i64) -> Result<()> {
    tx.execute(
        "INSERT INTO symbols(id, path, lang, kind, name, fqn, parent_id, start_byte, end_byte, \
             start_line, end_line, body_hash, own_hash, role, meta, disambiguator, updated_at, repo_id) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18) \
         ON CONFLICT(id) DO UPDATE SET lang=excluded.lang, parent_id=excluded.parent_id, \
             start_byte=excluded.start_byte, end_byte=excluded.end_byte, \
             start_line=excluded.start_line, end_line=excluded.end_line, \
             body_hash=excluded.body_hash, own_hash=excluded.own_hash, role=excluded.role, \
             meta=excluded.meta, updated_at=excluded.updated_at",
        params![
            row.id,
            row.path,
            row.lang,
            row.kind.as_str(),
            row.name,
            row.fqn,
            row.parent,
            row.start_byte,
            row.end_byte,
            row.start_line,
            row.end_line,
            row.body_hash,
            row.own_hash,
            row.role.map(|r| r.as_str()),
            row.meta.to_string(),
            row.disambiguator,
            now,
            row.repo_id,
        ],
    )?;
    Ok(())
}

fn upsert_file(
    tx: &Transaction,
    repo_id: &str,
    path: &str,
    lang: &str,
    content_hash: &str,
    size: u64,
    now: i64,
) -> Result<()> {
    tx.execute(
        "INSERT INTO files(repo_id, path, lang, content_hash, size, scanned_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
         ON CONFLICT(repo_id, path) DO UPDATE SET lang=excluded.lang, \
             content_hash=excluded.content_hash, size=excluded.size, scanned_at=excluded.scanned_at",
        params![repo_id, path, lang, content_hash, size as i64, now],
    )?;
    Ok(())
}

/// Drop symbols in `path` (within `repo_id`) that no longer exist, retiring
/// their leases first.
fn retire_missing_symbols(tx: &Transaction, repo_id: &str, path: &str, keep: &[String]) -> Result<usize> {
    let mut stmt = tx.prepare(
        "SELECT id, fqn FROM symbols WHERE repo_id = ?1 AND path = ?2 AND kind != 'service'",
    )?;
    let existing: Vec<(String, String)> = stmt
        .query_map(params![repo_id, path], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<Result<Vec<_>, _>>()?;
    drop(stmt);

    let keep_set: HashSet<&str> = keep.iter().map(|s| s.as_str()).collect();
    let mut dropped = 0usize;
    for (id, fqn) in existing {
        if keep_set.contains(id.as_str()) {
            continue;
        }
        dropped += retire_leases_on(tx, &id, &format!("{path}:{fqn}"), "symbol removed")?;
        tx.execute("DELETE FROM edges WHERE src = ?1 OR dst = ?1", params![id])?;
        tx.execute("DELETE FROM symbols WHERE id = ?1", params![id])?;
    }
    Ok(dropped)
}

fn retire_leases_on(tx: &Transaction, symbol_id: &str, handle: &str, reason: &str) -> Result<usize> {
    let mut stmt =
        tx.prepare("SELECT id, agent, task FROM leases WHERE symbol_id = ?1 AND state = 'active'")?;
    let active: Vec<(String, String, Option<String>)> = stmt
        .query_map(params![symbol_id], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    drop(stmt);

    for (lease_id, agent, task) in &active {
        tx.execute(
            "UPDATE leases SET state = 'expired', released_at = ?2 WHERE id = ?1",
            params![lease_id, ids::now_ms()],
        )?;
        store::emit(
            tx,
            "lease.dropped",
            Some(agent),
            Some(handle),
            task.as_deref(),
            json!({ "lease": lease_id, "reason": reason }),
        )?;
    }
    Ok(active.len())
}

fn remove_file(tx: &Transaction, repo_id: &str, path: &str) -> Result<usize> {
    let dropped = retire_missing_symbols(tx, repo_id, path, &[])?;
    tx.execute(
        "DELETE FROM files WHERE repo_id = ?1 AND path = ?2",
        params![repo_id, path],
    )?;
    store::emit(
        tx,
        "file.removed",
        None,
        Some(path),
        None,
        json!({ "path": path }),
    )?;
    Ok(dropped)
}

// -------------------------------------------------------------------- edges

/// Everything edge resolution needs to look at the repository as a whole.
struct EdgeContext {
    /// Symbol name -> (id, path), for call resolution.
    by_name: HashMap<String, Vec<(String, String)>>,
    /// Table name -> symbol id.
    tables: HashMap<String, String>,
    known_tables: BTreeSet<String>,
    /// Every indexed file, for import resolution.
    files: FileIndex,
}

impl EdgeContext {
    /// Scoped to one repo: names, tables and the file index only ever see
    /// this repo's rows, so a call or import can never accidentally resolve
    /// into a different repo that happens to share a relative path.
    fn build(store: &Store, repo_id: &str) -> Result<EdgeContext> {
        let mut by_name: HashMap<String, Vec<(String, String)>> = HashMap::new();
        let mut tables = HashMap::new();
        let mut known_tables = BTreeSet::new();

        let mut stmt = store.conn().prepare(
            "SELECT name, id, path, kind FROM symbols \
             WHERE kind NOT IN ('file', 'service') AND repo_id = ?1",
        )?;
        let rows = stmt.query_map(params![repo_id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
            ))
        })?;
        for row in rows {
            let (name, id, path, kind) = row?;
            if kind == SymbolKind::Table.as_str() {
                known_tables.insert(name.clone());
                tables.insert(name, id);
            } else {
                by_name.entry(name).or_default().push((id, path));
            }
        }
        drop(stmt);

        Ok(EdgeContext {
            by_name,
            tables,
            known_tables,
            files: FileIndex::new(store.indexed_files(repo_id)?),
        })
    }
}

/// Build every edge that originates in one file.
fn build_edges(f: &Indexed, repo_id: &str, ctx: &EdgeContext) -> Vec<Edge> {
    let mut edges: Vec<Edge> = Vec::new();

    // Containment.
    for (i, s) in f.pf.symbols.iter().enumerate() {
        let parent = match s.parent {
            Some(p) => f.sym_ids[p].clone(),
            None => f.file_id.clone(),
        };
        edges.push(Edge {
            src: parent,
            dst: f.sym_ids[i].clone(),
            kind: EdgeKind::Contains,
        });
    }

    // Imports, which also scope the call resolution below.
    let mut imported_files: Vec<String> = Vec::new();
    for imp in imports::extract_for_path(&f.path, &f.source) {
        for target in imports::resolve(f.pf.lang, &f.path, &imp.specifier, &ctx.files) {
            if target == f.path {
                continue;
            }
            imported_files.push(target.clone());
            edges.push(Edge {
                src: f.file_id.clone(),
                dst: ids::file_symbol_id(repo_id, &target),
                kind: EdgeKind::Imports,
            });
        }
    }
    let import_scope: HashSet<&str> = imported_files.iter().map(|s| s.as_str()).collect();

    // Calls and type references.
    for r in &f.pf.refs {
        let (src, src_role) = match r.from {
            Some(i) => (f.sym_ids[i].clone(), f.roles[i]),
            None => (f.file_id.clone(), None),
        };
        let Some(dst) = resolve_reference(&r.name, &f.path, &import_scope, ctx) else {
            continue;
        };
        if dst == src {
            continue;
        }
        // A call from a test is a test edge: "which tests cover this?" is a
        // question worth being able to ask directly.
        let kind = if src_role == Some(Role::Test) && r.kind == EdgeKind::Calls {
            EdgeKind::Tests
        } else {
            r.kind
        };
        edges.push(Edge { src, dst, kind });
    }

    // Database access, from SQL embedded in each symbol's own text.
    if !ctx.known_tables.is_empty() {
        if let Ok(text) = std::str::from_utf8(&f.source) {
            for (i, s) in f.pf.symbols.iter().enumerate() {
                let body = text.get(s.start_byte..s.end_byte.min(text.len())).unwrap_or("");
                for table in sql::referenced(body, &ctx.known_tables) {
                    if let Some(dst) = ctx.tables.get(&table) {
                        edges.push(Edge {
                            src: f.sym_ids[i].clone(),
                            dst: dst.clone(),
                            kind: EdgeKind::Queries,
                        });
                    }
                }
            }
        }
    }

    edges.sort_by(|a, b| {
        a.src
            .cmp(&b.src)
            .then(a.dst.cmp(&b.dst))
            .then(a.kind.as_str().cmp(b.kind.as_str()))
    });
    edges.dedup_by(|a, b| a.src == b.src && a.dst == b.dst && a.kind == b.kind);
    edges
}

/// Resolve a referenced name to a symbol id.
///
/// Preference order is the same one a reader uses: something defined here,
/// then something this file imported, then a repo-wide unique name. Anything
/// still ambiguous is left unresolved rather than guessed at.
fn resolve_reference(
    name: &str,
    from_path: &str,
    import_scope: &HashSet<&str>,
    ctx: &EdgeContext,
) -> Option<String> {
    let candidates = ctx.by_name.get(name)?;

    let local: Vec<&(String, String)> = candidates.iter().filter(|(_, p)| p == from_path).collect();
    if local.len() == 1 {
        return Some(local[0].0.clone());
    }
    if local.is_empty() {
        let imported: Vec<&(String, String)> = candidates
            .iter()
            .filter(|(_, p)| import_scope.contains(p.as_str()))
            .collect();
        if imported.len() == 1 {
            return Some(imported[0].0.clone());
        }
        if imported.is_empty() && candidates.len() == 1 {
            return Some(candidates[0].0.clone());
        }
    }
    None
}

/// A file importing another file makes its service depend on the other's.
fn roll_up_service_edges(tx: &Transaction) -> Result<usize> {
    tx.execute("DELETE FROM edges WHERE kind = 'imports' AND src IN (SELECT id FROM symbols WHERE kind = 'service')", [])?;
    let n = tx.execute(
        "INSERT OR IGNORE INTO edges(src, dst, kind) \
         SELECT DISTINCT src_svc.id, dst_svc.id, 'imports' \
         FROM edges e \
         JOIN symbols sf ON sf.id = e.src AND sf.kind = 'file' \
         JOIN symbols df ON df.id = e.dst AND df.kind = 'file' \
         JOIN symbols src_svc ON src_svc.id = sf.parent_id \
         JOIN symbols dst_svc ON dst_svc.id = df.parent_id \
         WHERE e.kind = 'imports' AND src_svc.id != dst_svc.id",
        [],
    )?;
    Ok(n)
}

// ------------------------------------------------------------------- walking

/// The indexable files in a tree, split by how they are read.
#[derive(Default)]
struct Found {
    sources: Vec<String>,
    sql: Vec<String>,
    manifests: Vec<String>,
}

impl Found {
    fn push(&mut self, rel: String) {
        if lang::for_path(&rel).is_some() {
            self.sources.push(rel);
        } else if sql::is_sql_path(&rel) {
            self.sql.push(rel);
        } else if topology::is_manifest(&rel) {
            self.manifests.push(rel);
        }
    }

    fn merge(&mut self, other: Found) {
        self.sources.extend(other.sources);
        self.sql.extend(other.sql);
        self.manifests.extend(other.manifests);
    }

    fn sort(&mut self) {
        self.sources.sort();
        self.sql.sort();
        self.manifests.sort();
    }
}

/// Repo-relative, forward-slashed path.
pub(crate) fn relative(root: &Path, abs: &Path) -> Option<String> {
    let rel = abs.strip_prefix(root).ok()?;
    Some(rel.to_string_lossy().replace('\\', "/"))
}

/// Walk `dir` honouring .gitignore, collecting everything the indexer reads.
fn collect(root: &Path, dir: &Path) -> Result<Found> {
    let mut found = Found::default();
    let walker = ignore::WalkBuilder::new(dir)
        .hidden(false)
        .git_ignore(true)
        .git_global(false)
        .require_git(false)
        .filter_entry(|e| {
            let name = e.file_name().to_string_lossy();
            !(name == crate::RUNTIME_DIR
                || name == ".git"
                || name == "node_modules"
                || name == "target"
                || name == "dist"
                || name == "build"
                || name == "__pycache__"
                || name == ".venv")
        })
        .build();

    for entry in walker {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        if entry.metadata().map(|m| m.len()).unwrap_or(0) > MAX_FILE_BYTES {
            continue;
        }
        if let Some(rel) = relative(root, entry.path()) {
            found.push(rel);
        }
    }
    found.sort();
    Ok(found)
}

/// Repo-relative source and SQL files — what enforcement walks.
pub(crate) fn collect_files(root: &Path, dir: &Path) -> Result<Vec<String>> {
    let found = collect(root, dir)?;
    Ok(found.sources)
}

/// Repo-relative form of an absolute path, or `None` if it is outside `root`.
pub fn relative_path(root: &Path, abs: &Path) -> Option<String> {
    relative(root, abs)
}

/// Would a scan read this file at all? Used by the watcher to ignore churn in
/// files the graph does not model.
pub fn is_indexable(rel: &str) -> bool {
    lang::for_path(rel).is_some() || sql::is_sql_path(rel) || topology::is_manifest(rel)
}

/// Read a file from the workspace as UTF-8 bytes.
pub fn read_workspace_file(root: &Path, rel: &str) -> Result<Vec<u8>> {
    let abs = root.join(rel);
    std::fs::read(&abs).with_context(|| format!("reading {}", abs.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace(files: &[(&str, &str)]) -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        for (name, body) in files {
            let p = dir.path().join(name);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, body).unwrap();
        }
        let store = Store::init(dir.path()).unwrap();
        (dir, store)
    }

    #[test]
    fn scan_indexes_symbols_and_containment() {
        let (dir, mut store) = workspace(&[(
            "src/pay.ts",
            "export class PaymentService {\n  processPayment(id: string) { return charge(id); }\n}\nexport function charge(id: string) { return 1; }\n",
        )]);
        let stats = scan(&mut store, ids::DEFAULT_REPO_ID, dir.path(), &[], false).unwrap();
        assert_eq!(stats.files_indexed, 1);

        let file = store.resolve("src/pay.ts").unwrap();
        assert_eq!(file.kind, SymbolKind::File);

        let method = store.resolve("PaymentService.processPayment").unwrap();
        assert_eq!(method.kind, SymbolKind::Method);

        let ancestors: Vec<String> = store
            .ancestors(&method.id)
            .unwrap()
            .into_iter()
            .map(|s| s.name)
            .collect();
        assert!(ancestors.contains(&"PaymentService".to_string()), "{ancestors:?}");
        assert!(ancestors.contains(&"pay.ts".to_string()), "{ancestors:?}");

        let descendants = store.descendants(&file.id).unwrap();
        assert!(descendants.iter().any(|s| s.name == "processPayment"));
        assert!(store.edge_count().unwrap() > 0);
    }

    #[test]
    fn rescanning_unchanged_files_is_a_no_op() {
        let (dir, mut store) = workspace(&[("a.py", "def f():\n    return 1\n")]);
        scan(&mut store, ids::DEFAULT_REPO_ID, dir.path(), &[], false).unwrap();
        let again = scan(&mut store, ids::DEFAULT_REPO_ID, dir.path(), &[], false).unwrap();
        assert_eq!(again.files_indexed, 0);
        assert_eq!(again.files_unchanged, 1);
    }

    #[test]
    fn ids_survive_body_edits_but_not_renames() {
        let (dir, mut store) = workspace(&[("a.py", "def f():\n    return 1\n")]);
        scan(&mut store, ids::DEFAULT_REPO_ID, dir.path(), &[], false).unwrap();
        let before = store.resolve("f").unwrap();

        std::fs::write(dir.path().join("a.py"), "def f():\n    return 999\n").unwrap();
        scan(&mut store, ids::DEFAULT_REPO_ID, dir.path(), &[], false).unwrap();
        let after = store.resolve("f").unwrap();
        assert_eq!(before.id, after.id, "identity must survive a body edit");
        assert_ne!(before.body_hash, after.body_hash);

        std::fs::write(dir.path().join("a.py"), "def g():\n    return 999\n").unwrap();
        scan(&mut store, ids::DEFAULT_REPO_ID, dir.path(), &[], false).unwrap();
        assert!(store.symbol(&before.id).unwrap().is_none(), "renamed symbol should be pruned");
        assert!(store.resolve("g").is_ok());
    }

    #[test]
    fn deleting_a_file_prunes_its_symbols() {
        let (dir, mut store) = workspace(&[("a.py", "def f():\n    pass\n")]);
        scan(&mut store, ids::DEFAULT_REPO_ID, dir.path(), &[], false).unwrap();
        std::fs::remove_file(dir.path().join("a.py")).unwrap();
        let stats = scan(&mut store, ids::DEFAULT_REPO_ID, dir.path(), &[], false).unwrap();
        assert_eq!(stats.files_removed, 1);
        assert_eq!(store.symbol_count().unwrap(), 0);
    }

    /// The payoff from resolving imports: two files defining `record`, and a
    /// call that lands on the one actually imported.
    #[test]
    fn imports_disambiguate_same_named_symbols_across_files() {
        let (dir, mut store) = workspace(&[
            ("src/ledger.ts", "export function record(x: number) { return x; }\n"),
            ("src/audit.ts", "export function record(x: number) { return -x; }\n"),
            (
                "src/pay.ts",
                "import { record } from './ledger';\nexport function charge(x: number) { return record(x); }\n",
            ),
        ]);
        scan(&mut store, ids::DEFAULT_REPO_ID, dir.path(), &[], false).unwrap();

        let charge = store.resolve("src/pay.ts:charge").unwrap();
        let callees = store.neighbors(&charge.id).unwrap().callees;
        let target = callees
            .iter()
            .find(|n| n.symbol.name == "record")
            .expect("record should resolve");
        assert_eq!(
            target.symbol.path, "src/ledger.ts",
            "the imported definition wins over the other one"
        );

        // And the file-level import edge exists in its own right.
        let pay = store.resolve("src/pay.ts").unwrap();
        let imports: Vec<String> = store
            .neighbors(&pay.id)
            .unwrap()
            .callees
            .iter()
            .filter(|n| n.kind == EdgeKind::Imports)
            .map(|n| n.symbol.path.clone())
            .collect();
        assert_eq!(imports, vec!["src/ledger.ts"]);
    }

    #[test]
    fn endpoints_and_tests_are_labelled_with_metadata() {
        let (dir, mut store) = workspace(&[
            (
                "src/api.py",
                "@app.get(\"/payments/{id}\")\ndef get_payment(id):\n    return 1\n",
            ),
            (
                "tests/test_api.py",
                "def test_get_payment():\n    return get_payment(1)\n",
            ),
        ]);
        let stats = scan(&mut store, ids::DEFAULT_REPO_ID, dir.path(), &[], false).unwrap();
        assert_eq!(stats.endpoints, 1);
        assert!(stats.tests >= 1);

        let endpoint = store.resolve("get_payment").unwrap();
        assert_eq!(endpoint.role, Some(Role::Api));
        assert_eq!(endpoint.route().as_deref(), Some("GET /payments/{id}"));

        // The test calls the handler, so it is recorded as a `tests` edge.
        let covering: Vec<String> = store
            .neighbors(&endpoint.id)
            .unwrap()
            .callers
            .iter()
            .filter(|n| n.kind == EdgeKind::Tests)
            .map(|n| n.symbol.name.clone())
            .collect();
        assert_eq!(covering, vec!["test_get_payment"]);
    }

    #[test]
    fn sql_tables_become_nodes_that_code_points_at() {
        let (dir, mut store) = workspace(&[
            (
                "db/schema.sql",
                "CREATE TABLE payments (id UUID PRIMARY KEY);\nCREATE TABLE audit_log (id UUID);\n",
            ),
            (
                "src/pay.ts",
                "export function charge(id: string) {\n  return db.query(\"INSERT INTO payments (id) VALUES ($1)\", [id]);\n}\n",
            ),
        ]);
        let stats = scan(&mut store, ids::DEFAULT_REPO_ID, dir.path(), &[], false).unwrap();
        assert_eq!(stats.tables, 2);

        let table = store.resolve("db/schema.sql:payments").unwrap();
        assert_eq!(table.kind, SymbolKind::Table);
        assert_eq!(table.role, Some(Role::Schema));

        let writers: Vec<String> = store
            .neighbors(&table.id)
            .unwrap()
            .callers
            .iter()
            .filter(|n| n.kind == EdgeKind::Queries)
            .map(|n| n.symbol.name.clone())
            .collect();
        assert_eq!(writers, vec!["charge"]);

        // The untouched table has no writers, which is also worth knowing.
        let audit = store.resolve("db/schema.sql:audit_log").unwrap();
        assert!(store.neighbors(&audit.id).unwrap().callers.is_empty());
    }

    #[test]
    fn manifests_become_services_that_own_their_files() {
        let (dir, mut store) = workspace(&[
            ("Cargo.toml", "[workspace]\nmembers = [\"crates/core\"]\n"),
            ("crates/core/Cargo.toml", "[package]\nname = \"core\"\n"),
            ("crates/core/src/lib.rs", "pub fn go() {}\n"),
            ("web/package.json", "{\"name\": \"web\"}"),
            ("web/index.ts", "export function boot() {}\n"),
        ]);
        let stats = scan(&mut store, ids::DEFAULT_REPO_ID, dir.path(), &[], false).unwrap();
        assert_eq!(stats.services, 2, "the workspace root declares no package");

        let core = store.resolve("service:core").unwrap();
        assert_eq!(core.kind, SymbolKind::Service);

        let lib = store.resolve("crates/core/src/lib.rs").unwrap();
        assert_eq!(lib.parent_id.as_deref(), Some(core.id.as_str()));

        // Leasing a service therefore covers everything inside it.
        let go = store.resolve("go").unwrap();
        let ancestors: Vec<String> = store
            .ancestors(&go.id)
            .unwrap()
            .into_iter()
            .map(|s| s.name)
            .collect();
        assert!(ancestors.contains(&"core".to_string()), "{ancestors:?}");
    }

    #[test]
    fn cross_service_imports_roll_up_to_service_dependencies() {
        let (dir, mut store) = workspace(&[
            ("api/package.json", "{\"name\": \"api\"}"),
            (
                "api/server.ts",
                "import { record } from '../lib/ledger';\nexport function boot() { return record(1); }\n",
            ),
            ("lib/package.json", "{\"name\": \"lib\"}"),
            ("lib/ledger.ts", "export function record(x: number) { return x; }\n"),
        ]);
        scan(&mut store, ids::DEFAULT_REPO_ID, dir.path(), &[], false).unwrap();

        let api = store.resolve("service:api").unwrap();
        let deps: Vec<String> = store
            .neighbors(&api.id)
            .unwrap()
            .callees
            .iter()
            .filter(|n| n.kind == EdgeKind::Imports)
            .map(|n| n.symbol.name.clone())
            .collect();
        assert_eq!(deps, vec!["lib"], "api depends on lib");
    }

    #[test]
    fn a_removed_manifest_retires_its_service() {
        let (dir, mut store) = workspace(&[
            ("web/package.json", "{\"name\": \"web\"}"),
            ("web/index.ts", "export function boot() {}\n"),
        ]);
        scan(&mut store, ids::DEFAULT_REPO_ID, dir.path(), &[], false).unwrap();
        assert!(store.resolve("service:web").is_ok());

        std::fs::remove_file(dir.path().join("web/package.json")).unwrap();
        scan(&mut store, ids::DEFAULT_REPO_ID, dir.path(), &[], false).unwrap();
        assert!(store.resolve("service:web").is_err());
        // The file survives; it just has no service any more.
        assert!(store.resolve("web/index.ts").unwrap().parent_id.is_none());
    }

    /// Two repos, sharing one `Store`, each with an identically-named
    /// relative path — the exact shape that would collide if any of the
    /// scoping in this module were missed.
    fn two_repo_workspace() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        for repo in ["repoA", "repoB"] {
            std::fs::create_dir_all(dir.path().join(repo).join("src")).unwrap();
            std::fs::write(
                dir.path().join(repo).join("src/index.ts"),
                format!("export function entry() {{ return '{repo}'; }}\n"),
            )
            .unwrap();
        }
        let mut store = Store::init(dir.path()).unwrap();
        store.add_repo("repoB", None).unwrap();
        (dir, store)
    }

    #[test]
    fn scanning_one_repo_does_not_delete_another_repos_symbols_or_leases() {
        let (dir, mut store) = two_repo_workspace();
        scan(&mut store, "R1", &dir.path().join("repoA"), &[], false).unwrap();
        scan(&mut store, "R2", &dir.path().join("repoB"), &[], false).unwrap();

        let b_entry = store.resolve("R2:src/index.ts:entry").unwrap();
        store
            .acquire(&b_entry.id, "agent-1", &crate::lease::AcquireOptions::default())
            .unwrap();

        // A full (re)scan of repo A alone must never touch repo B's rows.
        scan(&mut store, "R1", &dir.path().join("repoA"), &[], false).unwrap();

        assert!(
            store.symbol(&b_entry.id).unwrap().is_some(),
            "repo B's symbol must survive a full scan of repo A"
        );
        assert_eq!(
            store.active_leases(Some("agent-1")).unwrap().len(),
            1,
            "repo B's lease must survive a full scan of repo A"
        );
    }

    #[test]
    fn same_relative_path_in_two_repos_does_not_cross_resolve_calls() {
        let (dir, mut store) = two_repo_workspace();
        // repoA's entry calls a same-named function that only exists in repoA.
        std::fs::write(
            dir.path().join("repoA/src/index.ts"),
            "export function helper() { return 1; }\nexport function entry() { return helper(); }\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("repoB/src/index.ts"),
            "export function helper() { return 2; }\nexport function entry() { return helper(); }\n",
        )
        .unwrap();
        scan(&mut store, "R1", &dir.path().join("repoA"), &[], false).unwrap();
        scan(&mut store, "R2", &dir.path().join("repoB"), &[], false).unwrap();

        let a_entry = store.resolve("R1:src/index.ts:entry").unwrap();
        let a_callees: Vec<String> = store
            .neighbors(&a_entry.id)
            .unwrap()
            .callees
            .iter()
            .map(|n| n.symbol.repo_id.clone())
            .collect();
        assert_eq!(
            a_callees,
            vec!["R1"],
            "repoA's call must resolve to repoA's own helper, not repoB's"
        );
    }
}
