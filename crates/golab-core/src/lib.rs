//! # golab-core
//!
//! The collaborative agent runtime: a coordination layer that lets multiple
//! humans and AI agents work on one codebase without stepping on each other.
//!
//! The pieces, bottom up:
//!
//! - [`lang`] / [`parse`] — tree-sitter turns files into *symbols*
//!   (functions, methods, classes, modules), the unit of ownership.
//! - [`store`] — a SQLite database under `.golab/` holding the symbol graph,
//!   leases, agents, tasks, shared memory and the event log. Every mutation is
//!   a transaction, so independent processes coordinate safely.
//! - [`lease`] — time-bounded ownership of a symbol, with heartbeats, TTL
//!   expiry, containment-aware conflicts and a priority wait queue.
//! - [`check`] — enforcement: re-parse the working tree and reject edits to
//!   symbols the agent does not hold.
//! - [`protocol`] — agent-to-agent negotiation: typed requests with deadlines,
//!   atomic lease handover, and progress updates.
//! - [`schedule`] — the scheduler: dependency inference from the code graph,
//!   parallel waves, conflict-free claiming, automatic reassignment.
//! - [`work`] — agents, the task graph, shared memory and structured messages.

pub mod activity;
pub mod arch;
pub mod check;
pub mod context;
pub mod goal;
pub mod graph;
pub mod guard;
pub mod identity;
pub mod ids;
pub mod imports;
pub mod lang;
pub mod lease;
pub mod model;
pub mod notice;
pub mod notify;
pub mod parse;
pub mod protocol;
pub mod repo;
pub mod review;
pub mod roles;
pub mod schedule;
pub mod scan;
pub mod session;
pub mod sql;
pub mod store;
pub mod topology;
pub mod work;
pub mod workspace;

pub use ids::now_ms;
pub use model::*;
pub use store::Store;

/// Directory created by `golab init` at the workspace root.
pub const RUNTIME_DIR: &str = ".golab";
/// Database file inside [`RUNTIME_DIR`].
pub const DB_FILE: &str = "runtime.db";

use std::path::{Path, PathBuf};

/// Walk up from `start` looking for an initialized workspace.
pub fn find_workspace(start: &Path) -> Option<PathBuf> {
    let mut cur = Some(start);
    while let Some(dir) = cur {
        if dir.join(RUNTIME_DIR).join(DB_FILE).is_file() {
            return Some(dir.to_path_buf());
        }
        cur = dir.parent();
    }
    None
}

pub fn db_path(root: &Path) -> PathBuf {
    root.join(RUNTIME_DIR).join(DB_FILE)
}
