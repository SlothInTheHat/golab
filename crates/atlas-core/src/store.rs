//! SQLite-backed runtime state.
//!
//! Every process — CLI invocation, daemon, editor plugin — opens the same
//! database. Coordination correctness therefore rests on SQLite transactions
//! rather than on a server being up, which is what makes `atlas lease acquire`
//! safe to call from two agents at the same instant.

use std::path::Path;

use anyhow::{anyhow, Context, Result};
use rusqlite::{params, Connection, OptionalExtension, Row, Transaction, TransactionBehavior};
use serde_json::json;

use crate::ids;
use crate::model::*;

const SCHEMA: &str = r#"
PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS repos (
    id         TEXT PRIMARY KEY,
    name       TEXT NOT NULL,
    -- Relative to the directory containing .atlas/, so the workspace stays
    -- portable across machines and checkouts.
    root_path  TEXT NOT NULL,
    added_at   INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS files (
    repo_id      TEXT NOT NULL DEFAULT 'R1',
    path         TEXT NOT NULL,
    lang         TEXT NOT NULL,
    content_hash TEXT NOT NULL,
    size         INTEGER NOT NULL,
    scanned_at   INTEGER NOT NULL,
    PRIMARY KEY (repo_id, path)
);

CREATE TABLE IF NOT EXISTS symbols (
    id            TEXT PRIMARY KEY,
    path          TEXT NOT NULL,
    lang          TEXT NOT NULL,
    kind          TEXT NOT NULL,
    name          TEXT NOT NULL,
    fqn           TEXT NOT NULL,
    parent_id     TEXT,
    start_byte    INTEGER NOT NULL,
    end_byte      INTEGER NOT NULL,
    start_line    INTEGER NOT NULL,
    end_line      INTEGER NOT NULL,
    body_hash     TEXT NOT NULL,
    own_hash      TEXT NOT NULL DEFAULT '',
    role          TEXT,
    meta          TEXT NOT NULL DEFAULT '{}',
    disambiguator INTEGER NOT NULL DEFAULT 0,
    updated_at    INTEGER NOT NULL,
    -- Appended last, not inserted earlier: graph.rs reads joined-query
    -- columns by position, and an earlier insertion would silently shift them.
    repo_id       TEXT NOT NULL DEFAULT 'R1'
);
CREATE INDEX IF NOT EXISTS symbols_path   ON symbols(path);
CREATE INDEX IF NOT EXISTS symbols_role   ON symbols(role);
CREATE INDEX IF NOT EXISTS symbols_parent ON symbols(parent_id);
CREATE INDEX IF NOT EXISTS symbols_name   ON symbols(name);
CREATE INDEX IF NOT EXISTS symbols_fqn    ON symbols(fqn);
CREATE INDEX IF NOT EXISTS symbols_repo   ON symbols(repo_id);

CREATE TABLE IF NOT EXISTS edges (
    src  TEXT NOT NULL,
    dst  TEXT NOT NULL,
    kind TEXT NOT NULL,
    PRIMARY KEY (src, dst, kind)
);
CREATE INDEX IF NOT EXISTS edges_dst ON edges(dst);

CREATE TABLE IF NOT EXISTS agents (
    name         TEXT PRIMARY KEY,
    kind         TEXT NOT NULL,
    joined_at    INTEGER NOT NULL,
    heartbeat_at INTEGER NOT NULL,
    status       TEXT NOT NULL,
    -- A paused agent keeps its leases and its current task but is handed no
    -- new work. The pause button on the dashboard writes this.
    paused       INTEGER NOT NULL DEFAULT 0,
    -- NULL = generalist. Set, it's what `continue` matches a required
    -- capability against.
    capability   TEXT
);

CREATE TABLE IF NOT EXISTS leases (
    id           TEXT PRIMARY KEY,
    symbol_id    TEXT NOT NULL,
    agent        TEXT NOT NULL,
    task         TEXT,
    priority     INTEGER NOT NULL,
    ttl_secs     INTEGER NOT NULL,
    acquired_at  INTEGER NOT NULL,
    heartbeat_at INTEGER NOT NULL,
    expires_at   INTEGER NOT NULL,
    state        TEXT NOT NULL,
    note         TEXT,
    released_at  INTEGER
);
CREATE INDEX IF NOT EXISTS leases_symbol ON leases(symbol_id, state);
CREATE INDEX IF NOT EXISTS leases_agent  ON leases(agent, state);

CREATE TABLE IF NOT EXISTS lease_queue (
    id           TEXT PRIMARY KEY,
    seq          INTEGER NOT NULL,
    symbol_id    TEXT NOT NULL,
    agent        TEXT NOT NULL,
    task         TEXT,
    priority     INTEGER NOT NULL,
    ttl_secs     INTEGER NOT NULL,
    requested_at INTEGER NOT NULL,
    polled_at    INTEGER NOT NULL,
    state        TEXT NOT NULL,
    note         TEXT
);
CREATE INDEX IF NOT EXISTS queue_symbol ON lease_queue(symbol_id, state);

CREATE TABLE IF NOT EXISTS tasks (
    id                  TEXT PRIMARY KEY,
    title               TEXT NOT NULL,
    state               TEXT NOT NULL,
    priority            INTEGER NOT NULL,
    assignee            TEXT,
    claimed_at          INTEGER,
    created_at          INTEGER NOT NULL,
    updated_at          INTEGER NOT NULL,
    -- NULL = anyone can take it. Set, `continue` only offers it to an agent
    -- with a matching capability; `assign` bypasses this (human override).
    required_capability TEXT
);

-- Which symbols a task is expected to touch. This is what lets the scheduler
-- infer dependencies from the code graph and refuse to hand two agents work
-- that would collide.
CREATE TABLE IF NOT EXISTS task_symbols (
    task_id   TEXT NOT NULL,
    symbol_id TEXT NOT NULL,
    PRIMARY KEY (task_id, symbol_id)
);
CREATE INDEX IF NOT EXISTS task_symbols_symbol ON task_symbols(symbol_id);

CREATE TABLE IF NOT EXISTS task_deps (
    task_id    TEXT NOT NULL,
    depends_on TEXT NOT NULL,
    PRIMARY KEY (task_id, depends_on)
);

CREATE TABLE IF NOT EXISTS events (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    ts            INTEGER NOT NULL,
    kind          TEXT NOT NULL,
    agent         TEXT,
    symbol_handle TEXT,
    task          TEXT,
    detail        TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS events_ts ON events(ts);

CREATE TABLE IF NOT EXISTS memory (
    key        TEXT PRIMARY KEY,
    value      TEXT NOT NULL,
    author     TEXT,
    tags       TEXT NOT NULL DEFAULT '',
    updated_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS messages (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    from_agent TEXT NOT NULL,
    to_agent   TEXT NOT NULL,
    subject    TEXT NOT NULL,
    body       TEXT NOT NULL,
    sent_at    INTEGER NOT NULL,
    read_at    INTEGER,
    reply_to   INTEGER
);
CREATE INDEX IF NOT EXISTS messages_to ON messages(to_agent, read_at);

CREATE TABLE IF NOT EXISTS requests (
    id              TEXT PRIMARY KEY,
    seq             INTEGER NOT NULL,
    kind            TEXT NOT NULL,
    from_agent      TEXT NOT NULL,
    to_agent        TEXT,
    subject         TEXT NOT NULL,
    body            TEXT NOT NULL,
    resource_symbol TEXT,
    resource_task   TEXT,
    task            TEXT,
    priority        INTEGER NOT NULL,
    state           TEXT NOT NULL,
    created_at      INTEGER NOT NULL,
    deadline_at     INTEGER,
    resolved_at     INTEGER,
    resolver        TEXT,
    response        TEXT
);
CREATE INDEX IF NOT EXISTS requests_to     ON requests(to_agent, state);
CREATE INDEX IF NOT EXISTS requests_from   ON requests(from_agent, state);
CREATE INDEX IF NOT EXISTS requests_symbol ON requests(resource_symbol, state);
CREATE INDEX IF NOT EXISTS requests_task   ON requests(resource_task, state);

CREATE TABLE IF NOT EXISTS progress (
    id        INTEGER PRIMARY KEY AUTOINCREMENT,
    agent     TEXT NOT NULL,
    task      TEXT,
    symbol_id TEXT,
    percent   INTEGER,
    eta_secs  INTEGER,
    note      TEXT,
    ts        INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS progress_agent ON progress(agent, ts);

CREATE TABLE IF NOT EXISTS goals (
    id          TEXT PRIMARY KEY,
    title       TEXT NOT NULL,
    description TEXT,
    state       TEXT NOT NULL,
    priority    INTEGER NOT NULL,
    created_at  INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL,
    created_by  TEXT
);

-- Which goal a task serves. A join table, not a column on `tasks` — like
-- task_symbols and task_deps, it self-heals on an existing .atlas/runtime.db
-- (a new CREATE TABLE just appears), where ALTER TABLE ADD COLUMN would not.
CREATE TABLE IF NOT EXISTS task_goals (
    task_id TEXT PRIMARY KEY,
    goal_id TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS task_goals_goal ON task_goals(goal_id);

-- A live connection from a coding tool. A table rather than columns on
-- `agents` for the same self-healing reason as task_goals above, and because
-- one agent legitimately holds several at once — an MCP server and an editor
-- hook inside the same window. Collapsing that into a column would also
-- destroy the distinction a dashboard needs most: a stale `agents` row versus
-- a tool that is attached right now.
CREATE TABLE IF NOT EXISTS sessions (
    id           TEXT PRIMARY KEY,
    agent        TEXT NOT NULL,
    -- 'claude-code', 'cursor', 'ci', ... free-form, like agents.kind.
    tool         TEXT NOT NULL,
    -- 'mcp' | 'hook' | 'cli'
    transport    TEXT NOT NULL,
    -- The host's own session id, when it tells us one: the only thing tying a
    -- separately-spawned editor hook back to the session that started it.
    client_key   TEXT,
    cwd          TEXT NOT NULL,
    pid          INTEGER,
    started_at   INTEGER NOT NULL,
    heartbeat_at INTEGER NOT NULL,
    ended_at     INTEGER
);
CREATE INDEX IF NOT EXISTS sessions_agent  ON sessions(agent, ended_at);
CREATE INDEX IF NOT EXISTS sessions_client ON sessions(client_key);

-- What someone is editing *right now*, as opposed to what they hold (leases)
-- or how far along they say they are (progress). A third table rather than
-- more columns on either, because the lifetimes differ: a lease outlives the
-- edit that motivated it, a progress report is a point in time, and this is a
-- window that opens when a tool asks permission and closes when it goes quiet.
--
-- One row per (agent, repo, path) — an UPSERT, not an append. A tool firing a
-- PostToolUse callback per edit would otherwise write a row per keystroke
-- batch, and nobody wants to page through that to answer "who is in this file".
CREATE TABLE IF NOT EXISTS activity (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    agent         TEXT NOT NULL,
    session_id    TEXT,
    repo_id       TEXT NOT NULL,
    path          TEXT NOT NULL,
    symbol_id     TEXT,
    symbol_handle TEXT,
    -- 'opened' | 'editing' | 'edited' | 'blocked'
    kind          TEXT NOT NULL,
    task          TEXT,
    -- The guard verdict at that moment: 'allowed' | 'warn' | 'denied'.
    verdict       TEXT,
    note          TEXT,
    started_at    INTEGER NOT NULL,
    updated_at    INTEGER NOT NULL,
    UNIQUE(agent, repo_id, path)
);
CREATE INDEX IF NOT EXISTS activity_live ON activity(updated_at);
CREATE INDEX IF NOT EXISTS activity_path ON activity(repo_id, path, updated_at);
CREATE INDEX IF NOT EXISTS activity_sym  ON activity(symbol_id);
"#;

/// Bump when a change to [`SCHEMA`] **cannot** self-heal on an existing
/// database — that is, anything other than a fresh `CREATE TABLE`/`CREATE
/// INDEX ... IF NOT EXISTS`.
///
/// There are no migrations here on purpose, which works only as long as every
/// schema change is additive in that narrow sense. Adding a column to an
/// existing table is the trap: `CREATE TABLE IF NOT EXISTS` silently does
/// nothing, the column never appears, and the failure surfaces much later as a
/// confusing SQL error. This marker turns that into a clear message at open
/// time. Prefer a new table over a new column and you will never need to bump
/// it — see the `task_goals` and `sessions` comments above.
pub const SCHEMA_VERSION: i64 = 1;

pub(crate) const SYMBOL_COLS: &str =
    "id, path, lang, kind, name, fqn, parent_id, start_byte, end_byte, \
     start_line, end_line, body_hash, own_hash, role, meta, repo_id";

/// The same columns, qualified for queries that join `symbols AS s`. `repo_id`
/// stays last: `graph.rs` reads a trailing joined `e.kind` column by position,
/// and appending here (rather than inserting) keeps that index stable.
pub(crate) const QUALIFIED_COLS: &str =
    "s.id, s.path, s.lang, s.kind, s.name, s.fqn, s.parent_id, s.start_byte, s.end_byte, \
     s.start_line, s.end_line, s.body_hash, s.own_hash, s.role, s.meta, s.repo_id";

pub struct Store {
    conn: Connection,
}

impl Store {
    /// Create `.atlas/runtime.db` under `root` (idempotent).
    pub fn init(root: &Path) -> Result<Store> {
        let dir = root.join(crate::RUNTIME_DIR);
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        // Indexed state is derived; never worth committing.
        let ignore = dir.join(".gitignore");
        if !ignore.exists() {
            std::fs::write(&ignore, "*\n")?;
        }
        let mut store = Store::open(&crate::db_path(root))?;
        store.conn.execute(
            "INSERT OR IGNORE INTO meta(key, value) VALUES ('created_at', ?1)",
            params![ids::now_ms().to_string()],
        )?;
        store.ensure_default_repo()?;
        Ok(store)
    }

    pub fn open(db: &Path) -> Result<Store> {
        if let Some(parent) = db.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let conn = Connection::open(db).with_context(|| format!("opening {}", db.display()))?;
        Store::configure(conn)
    }

    pub fn open_in_memory() -> Result<Store> {
        Store::configure(Connection::open_in_memory()?)
    }

    fn configure(conn: Connection) -> Result<Store> {
        // WAL + a generous busy timeout: concurrent agents block briefly
        // instead of failing with SQLITE_BUSY.
        conn.pragma_update(None, "journal_mode", "WAL").ok();
        conn.pragma_update(None, "synchronous", "NORMAL").ok();
        conn.busy_timeout(std::time::Duration::from_secs(10))?;
        conn.execute_batch(SCHEMA)?;
        let store = Store { conn };
        store.enforce_schema_version()?;
        Ok(store)
    }

    /// What schema this database was written for. `SCHEMA_VERSION` for
    /// anything created before the marker existed — every change up to that
    /// point was a new table, which `CREATE TABLE IF NOT EXISTS` just adds.
    pub fn schema_version(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'schema_version'",
                [],
                |r| r.get::<_, String>(0),
            )
            .optional()?
            .and_then(|s| s.parse().ok())
            .unwrap_or(SCHEMA_VERSION))
    }

    /// Fail loudly when the binary and the database disagree about what is in
    /// there.
    ///
    /// An error rather than a warning, in both directions. A version bump means
    /// a change that `CREATE TABLE IF NOT EXISTS` could not apply, so an older
    /// binary genuinely cannot read a newer database and a newer binary cannot
    /// trust an older one. And a warning would be worse than useless here: the
    /// processes most likely to hit this — the daemon, an MCP adapter — write
    /// their stderr to a log nobody reads, so the only failure anyone would
    /// ever see is silently wrong coordination.
    fn enforce_schema_version(&self) -> Result<()> {
        let stored: Option<i64> = self
            .conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'schema_version'",
                [],
                |r| r.get::<_, String>(0),
            )
            .optional()?
            .and_then(|s| s.parse().ok());

        match stored {
            Some(v) if v != SCHEMA_VERSION => Err(anyhow!(
                "this workspace's .atlas/runtime.db was written for schema v{v}, but this atlas \
                 speaks v{SCHEMA_VERSION}. There are no migrations by design — delete \
                 .atlas/runtime.db and re-run `atlas index` to rebuild it."
            )),
            Some(_) => Ok(()),
            None => {
                self.conn.execute(
                    "INSERT OR IGNORE INTO meta(key, value) VALUES ('schema_version', ?1)",
                    params![SCHEMA_VERSION.to_string()],
                )?;
                Ok(())
            }
        }
    }

    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    /// Run `f` inside an IMMEDIATE transaction, so writers serialize from the
    /// first statement rather than upgrading mid-flight.
    pub fn write<T>(&mut self, f: impl FnOnce(&Transaction) -> Result<T>) -> Result<T> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let out = f(&tx)?;
        tx.commit()?;
        Ok(out)
    }

    // ---------------------------------------------------------------- files

    pub fn file_hash(&self, repo_id: &str, path: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT content_hash FROM files WHERE repo_id = ?1 AND path = ?2",
                params![repo_id, path],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// Files indexed for one repo. Scoped deliberately: a full scan of one
    /// repo must never treat another repo's files as "no longer present" and
    /// prune them, and `EdgeContext` must never wire a call across repos just
    /// because they happen to share a relative path.
    pub fn indexed_files(&self, repo_id: &str) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT path FROM files WHERE repo_id = ?1 ORDER BY path")?;
        let rows = stmt.query_map(params![repo_id], |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    pub fn file_count(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))?)
    }

    // -------------------------------------------------------------- symbols

    pub fn symbol(&self, id: &str) -> Result<Option<Symbol>> {
        Ok(self
            .conn
            .query_row(
                &format!("SELECT {SYMBOL_COLS} FROM symbols WHERE id = ?1"),
                params![id],
                row_to_symbol,
            )
            .optional()?)
    }

    pub fn symbols_in_file(&self, repo_id: &str, path: &str) -> Result<Vec<Symbol>> {
        self.query_symbols(
            &format!(
                "SELECT {SYMBOL_COLS} FROM symbols WHERE repo_id = ?1 AND path = ?2 ORDER BY start_byte"
            ),
            params![repo_id, path],
        )
    }

    pub fn symbol_count(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("SELECT COUNT(*) FROM symbols", [], |r| r.get(0))?)
    }

    /// List symbols with optional filters. `pattern` matches name, fqn or path.
    pub fn list_symbols(
        &self,
        path: Option<&str>,
        kind: Option<SymbolKind>,
        pattern: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Symbol>> {
        let mut sql = format!("SELECT {SYMBOL_COLS} FROM symbols WHERE 1=1");
        let mut args: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        if let Some(p) = path {
            sql.push_str(" AND path = ?");
            args.push(Box::new(p.to_string()));
        }
        if let Some(k) = kind {
            sql.push_str(" AND kind = ?");
            args.push(Box::new(k.as_str().to_string()));
        }
        if let Some(pat) = pattern {
            sql.push_str(" AND (name LIKE ? OR fqn LIKE ? OR path LIKE ?)");
            let like = format!("%{pat}%");
            args.push(Box::new(like.clone()));
            args.push(Box::new(like.clone()));
            args.push(Box::new(like));
        }
        sql.push_str(" ORDER BY path, start_byte LIMIT ?");
        args.push(Box::new(limit as i64));

        let mut stmt = self.conn.prepare(&sql)?;
        let refs: Vec<&dyn rusqlite::ToSql> = args.iter().map(|b| b.as_ref()).collect();
        let rows = stmt.query_map(refs.as_slice(), row_to_symbol)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Resolve what a human or agent typed into exactly one symbol.
    ///
    /// Accepted forms, in precedence order: symbol id, `path:Fqn`, a file
    /// path, a fully qualified name, a bare symbol name, then a fuzzy match.
    pub fn resolve(&self, query: &str) -> Result<Symbol> {
        let candidates = self.resolve_all(query)?;
        match candidates.len() {
            0 => Err(anyhow!(
                "no symbol matches '{query}' (run `atlas scan` if the index is stale)"
            )),
            1 => Ok(candidates.into_iter().next().expect("len 1")),
            _ => {
                let mut msg = format!("'{query}' is ambiguous, {} matches:\n", candidates.len());
                for c in candidates.iter().take(10) {
                    let handle = crate::repo::qualified_handle(self, c).unwrap_or_else(|_| c.handle());
                    msg.push_str(&format!("  {} [{}]\n", handle, c.kind.as_str()));
                }
                msg.push_str("disambiguate with repo:path:Fqn or the symbol id");
                Err(anyhow!(msg))
            }
        }
    }

    /// All symbols a query could refer to, most specific interpretation first.
    pub fn resolve_all(&self, query: &str) -> Result<Vec<Symbol>> {
        let q = query.trim();
        if q.is_empty() {
            return Ok(vec![]);
        }
        if let Some(s) = self.symbol(q)? {
            return Ok(vec![s]);
        }

        let norm = q.replace('\\', "/");
        // `service:payments` — the handle a Service symbol reports.
        if let Some(name) = norm.strip_prefix("service:") {
            let found = self.query_symbols(
                &format!("SELECT {SYMBOL_COLS} FROM symbols WHERE kind = 'service' AND name = ?1"),
                params![name],
            )?;
            if !found.is_empty() {
                return Ok(found);
            }
        }
        // `repo:path:Fqn` — an explicit repo qualifier ahead of the usual
        // `path:Fqn` form, for disambiguating a path/fqn shared across two
        // repos. Peels off the first colon rather than the last, since the
        // remainder can itself contain colons the way any `path:Fqn` might.
        if let Some((prefix, rest)) = norm.split_once(':') {
            if let Some(repo_id) = self.resolve_repo_id(prefix)? {
                if let Some((path, fqn)) = rest.rsplit_once(':') {
                    let found = self.query_symbols(
                        &format!(
                            "SELECT {SYMBOL_COLS} FROM symbols WHERE repo_id = ?1 AND path = ?2 AND fqn = ?3"
                        ),
                        params![repo_id, path, fqn],
                    )?;
                    if !found.is_empty() {
                        return Ok(found);
                    }
                }
            }
        }
        if let Some((path, fqn)) = norm.rsplit_once(':') {
            let found = self.query_symbols(
                &format!("SELECT {SYMBOL_COLS} FROM symbols WHERE path = ?1 AND fqn = ?2"),
                params![path, fqn],
            )?;
            if !found.is_empty() {
                return Ok(found);
            }
        }
        let found = self.query_symbols(
            &format!("SELECT {SYMBOL_COLS} FROM symbols WHERE path = ?1 AND kind = 'file'"),
            params![norm],
        )?;
        if !found.is_empty() {
            return Ok(found);
        }
        for col in ["fqn", "name"] {
            let found = self.query_symbols(
                &format!("SELECT {SYMBOL_COLS} FROM symbols WHERE {col} = ?1"),
                params![norm],
            )?;
            if !found.is_empty() {
                return Ok(found);
            }
        }
        self.query_symbols(
            &format!(
                "SELECT {SYMBOL_COLS} FROM symbols WHERE fqn LIKE ?1 OR path LIKE ?1 LIMIT 25"
            ),
            params![format!("%{norm}%")],
        )
    }

    fn query_symbols(&self, sql: &str, args: impl rusqlite::Params) -> Result<Vec<Symbol>> {
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt.query_map(args, row_to_symbol)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// The symbol plus every enclosing scope, innermost first.
    pub fn ancestors(&self, id: &str) -> Result<Vec<Symbol>> {
        let mut out = Vec::new();
        let mut cur = self.symbol(id)?;
        while let Some(s) = cur {
            let parent = s.parent_id.clone();
            out.push(s);
            cur = match parent {
                Some(p) => self.symbol(&p)?,
                None => None,
            };
        }
        Ok(out)
    }

    /// Every symbol nested inside `id` (excluding `id` itself).
    pub fn descendants(&self, id: &str) -> Result<Vec<Symbol>> {
        self.query_symbols(
            &format!(
                "WITH RECURSIVE sub(id) AS (
                     SELECT id FROM symbols WHERE parent_id = ?1
                     UNION ALL
                     SELECT s.id FROM symbols s JOIN sub ON s.parent_id = sub.id
                 )
                 SELECT {SYMBOL_COLS} FROM symbols WHERE id IN (SELECT id FROM sub)"
            ),
            params![id],
        )
    }

    /// Services as recorded by the last full scan of one repo. A partial scan
    /// reads these rather than re-walking the tree for manifests.
    pub fn services(&self, repo_id: &str) -> Result<Vec<crate::topology::Service>> {
        let mut stmt = self.conn().prepare(
            "SELECT name, path, meta FROM symbols WHERE kind = 'service' AND repo_id = ?1 ORDER BY path",
        )?;
        let rows = stmt.query_map(params![repo_id], |r| {
            let meta: String = r.get(2)?;
            let meta: serde_json::Value = serde_json::from_str(&meta).unwrap_or_else(|_| json!({}));
            Ok(crate::topology::Service {
                name: r.get(0)?,
                manifest: r.get(1)?,
                dir: meta["dir"].as_str().unwrap_or("").to_string(),
                ecosystem: meta["ecosystem"].as_str().unwrap_or("").to_string(),
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Symbols carrying a given role — the endpoint list, the test list.
    pub fn symbols_with_role(&self, role: Role) -> Result<Vec<Symbol>> {
        self.query_symbols(
            &format!("SELECT {SYMBOL_COLS} FROM symbols WHERE role = ?1 ORDER BY path, start_byte"),
            params![role.as_str()],
        )
    }

    // ---------------------------------------------------------------- edges

    pub fn edge_count(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("SELECT COUNT(*) FROM edges", [], |r| r.get(0))?)
    }

    // --------------------------------------------------------------- events

    pub fn events_since(&self, after_id: i64, limit: usize) -> Result<Vec<Event>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, ts, kind, agent, symbol_handle, task, detail FROM events \
             WHERE id > ?1 ORDER BY id LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![after_id, limit as i64], row_to_event)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    pub fn recent_events(&self, limit: usize) -> Result<Vec<Event>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, ts, kind, agent, symbol_handle, task, detail FROM events \
             ORDER BY id DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit as i64], row_to_event)?;
        let mut v = rows.collect::<Result<Vec<_>, _>>()?;
        v.reverse();
        Ok(v)
    }

    pub fn last_event_id(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("SELECT COALESCE(MAX(id), 0) FROM events", [], |r| r.get(0))?)
    }
}

/// Append to the event log. Everything interesting in the runtime goes through
/// here, so `atlas watch`, the dashboard and audits all see the same stream.
pub fn emit(
    tx: &Transaction,
    kind: &str,
    agent: Option<&str>,
    symbol_handle: Option<&str>,
    task: Option<&str>,
    detail: serde_json::Value,
) -> Result<i64> {
    tx.execute(
        "INSERT INTO events(ts, kind, agent, symbol_handle, task, detail) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            ids::now_ms(),
            kind,
            agent,
            symbol_handle,
            task,
            detail.to_string()
        ],
    )?;
    Ok(tx.last_insert_rowid())
}

pub(crate) fn row_to_symbol(r: &Row) -> rusqlite::Result<Symbol> {
    let kind: String = r.get(3)?;
    Ok(Symbol {
        id: r.get(0)?,
        path: r.get(1)?,
        lang: r.get(2)?,
        kind: SymbolKind::parse(&kind).unwrap_or(SymbolKind::Function),
        name: r.get(4)?,
        fqn: r.get(5)?,
        parent_id: r.get(6)?,
        start_byte: r.get::<_, i64>(7)? as usize,
        end_byte: r.get::<_, i64>(8)? as usize,
        start_line: r.get::<_, i64>(9)? as usize,
        end_line: r.get::<_, i64>(10)? as usize,
        body_hash: r.get(11)?,
        own_hash: r.get(12)?,
        role: r.get::<_, Option<String>>(13)?.and_then(|s| Role::parse(&s)),
        meta: r
            .get::<_, String>(14)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_else(|| json!({})),
        repo_id: r.get(15)?,
    })
}

pub(crate) fn row_to_event(r: &Row) -> rusqlite::Result<Event> {
    let detail: String = r.get(6)?;
    Ok(Event {
        id: r.get(0)?,
        ts: r.get(1)?,
        kind: r.get(2)?,
        agent: r.get(3)?,
        symbol_handle: r.get(4)?,
        task: r.get(5)?,
        detail: serde_json::from_str(&detail).unwrap_or_else(|_| json!({})),
    })
}

/// Symbol lookup usable inside a transaction.
pub(crate) fn tx_symbol(tx: &Transaction, id: &str) -> Result<Option<Symbol>> {
    Ok(tx
        .query_row(
            &format!("SELECT {SYMBOL_COLS} FROM symbols WHERE id = ?1"),
            params![id],
            row_to_symbol,
        )
        .optional()?)
}

/// Ids of `id` and every enclosing scope, inside a transaction.
pub(crate) fn tx_ancestor_ids(tx: &Transaction, id: &str) -> Result<Vec<String>> {
    let mut out = vec![id.to_string()];
    let mut cur = id.to_string();
    // Depth is bounded by nesting depth; the guard is paranoia about cycles.
    for _ in 0..64 {
        let parent: Option<String> = tx
            .query_row(
                "SELECT parent_id FROM symbols WHERE id = ?1",
                params![cur],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        match parent {
            Some(p) => {
                out.push(p.clone());
                cur = p;
            }
            None => break,
        }
    }
    Ok(out)
}

/// Ids of every symbol nested inside `id`, inside a transaction.
pub(crate) fn tx_descendant_ids(tx: &Transaction, id: &str) -> Result<Vec<String>> {
    let mut stmt = tx.prepare(
        "WITH RECURSIVE sub(id) AS (
             SELECT id FROM symbols WHERE parent_id = ?1
             UNION ALL
             SELECT s.id FROM symbols s JOIN sub ON s.parent_id = sub.id
         )
         SELECT id FROM sub",
    )?;
    let rows = stmt.query_map(params![id], |r| r.get::<_, String>(0))?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("runtime.db");
        Store::open(&db).unwrap();
        let s = Store::open(&db).unwrap();
        assert_eq!(s.symbol_count().unwrap(), 0);
    }

    #[test]
    fn a_fresh_database_is_stamped_with_the_current_schema() {
        let s = Store::open_in_memory().unwrap();
        assert_eq!(s.schema_version().unwrap(), SCHEMA_VERSION);
    }

    #[test]
    fn a_database_from_a_different_schema_refuses_to_open() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("runtime.db");
        {
            let s = Store::open(&db).unwrap();
            s.conn()
                .execute(
                    "UPDATE meta SET value = ?1 WHERE key = 'schema_version'",
                    params![(SCHEMA_VERSION + 1).to_string()],
                )
                .unwrap();
        }

        let msg = match Store::open(&db) {
            Ok(_) => panic!("a schema mismatch must not open silently"),
            Err(e) => format!("{e:#}"),
        };
        assert!(msg.contains("no migrations"), "{msg}");
        assert!(
            msg.contains("delete .atlas/runtime.db"),
            "the error has to say what to do about it: {msg}"
        );
    }

    #[test]
    fn a_database_from_before_the_marker_is_treated_as_current() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("runtime.db");
        {
            let s = Store::open(&db).unwrap();
            // Every schema change up to the marker was a new table, which
            // CREATE TABLE IF NOT EXISTS adds on its own. Those databases are
            // fine and must not be condemned.
            s.conn()
                .execute("DELETE FROM meta WHERE key = 'schema_version'", [])
                .unwrap();
        }

        let s = Store::open(&db).expect("an unstamped database is not a broken one");
        assert_eq!(s.schema_version().unwrap(), SCHEMA_VERSION);
    }

    #[test]
    fn events_round_trip() {
        let mut s = Store::open_in_memory().unwrap();
        s.write(|tx| {
            emit(
                tx,
                "agent.joined",
                Some("a1"),
                None,
                None,
                json!({"kind":"claude"}),
            )?;
            emit(tx, "agent.joined", Some("a2"), None, None, json!({}))?;
            Ok(())
        })
        .unwrap();
        let evs = s.events_since(0, 10).unwrap();
        assert_eq!(evs.len(), 2);
        assert_eq!(evs[0].agent.as_deref(), Some("a1"));
        assert_eq!(evs[0].detail["kind"], "claude");
        assert_eq!(s.events_since(evs[0].id, 10).unwrap().len(), 1);
    }
}
