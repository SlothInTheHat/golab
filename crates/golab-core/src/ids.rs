//! Stable identifiers.
//!
//! A symbol id must survive edits to the symbol's *body* — otherwise a lease
//! would evaporate the moment the agent holding it typed a character. So ids
//! are derived from identity (path + kind + qualified name), never content.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::model::SymbolKind;

fn short(hasher: blake3::Hasher) -> String {
    hasher.finalize().to_hex()[..16].to_string()
}

/// The repo every workspace has before `golab repo add` registers another.
pub const DEFAULT_REPO_ID: &str = "R1";

/// `repo_id` is hashed first so identity is unique across repos, not just
/// within one. The domain prefix is bumped `v1` -> `v2` for this: there are
/// no schema migrations (a `repo_id` column forces a fresh database anyway),
/// so the bump isn't a data migration — it guards against a daemon on the old
/// binary and a CLI on the new one computing colliding ids against the same
/// live `runtime.db` mid-deploy.
pub fn symbol_id(repo_id: &str, path: &str, kind: SymbolKind, fqn: &str, disambiguator: u32) -> String {
    let mut h = blake3::Hasher::new();
    h.update(b"golab.symbol.v2\0");
    h.update(repo_id.as_bytes());
    h.update(b"\0");
    h.update(path.as_bytes());
    h.update(b"\0");
    h.update(kind.as_str().as_bytes());
    h.update(b"\0");
    h.update(fqn.as_bytes());
    h.update(b"\0");
    h.update(&disambiguator.to_le_bytes());
    format!("s_{}", short(h))
}

pub fn file_symbol_id(repo_id: &str, path: &str) -> String {
    symbol_id(repo_id, path, SymbolKind::File, path, 0)
}

pub fn content_hash(bytes: &[u8]) -> String {
    let mut h = blake3::Hasher::new();
    h.update(bytes);
    short(h)
}

/// Monotonic-ish unique id for leases, tasks and other runtime objects.
pub fn unique_id(prefix: &str) -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let mut h = blake3::Hasher::new();
    h.update(&nanos.to_le_bytes());
    h.update(&n.to_le_bytes());
    h.update(&std::process::id().to_le_bytes());
    format!("{prefix}_{}", &short(h)[..12])
}

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn symbol_ids_are_identity_not_content() {
        let a = symbol_id(DEFAULT_REPO_ID, "src/pay.ts", SymbolKind::Function, "processPayment", 0);
        let b = symbol_id(DEFAULT_REPO_ID, "src/pay.ts", SymbolKind::Function, "processPayment", 0);
        assert_eq!(a, b);
        assert_ne!(a, symbol_id(DEFAULT_REPO_ID, "src/pay.ts", SymbolKind::Method, "processPayment", 0));
        assert_ne!(a, symbol_id(DEFAULT_REPO_ID, "src/other.ts", SymbolKind::Function, "processPayment", 0));
        assert_ne!(a, symbol_id(DEFAULT_REPO_ID, "src/pay.ts", SymbolKind::Function, "processPayment", 1));
    }

    #[test]
    fn symbol_id_differs_by_repo_id_alone() {
        let a = symbol_id("R1", "src/pay.ts", SymbolKind::Function, "processPayment", 0);
        let b = symbol_id("R2", "src/pay.ts", SymbolKind::Function, "processPayment", 0);
        assert_ne!(a, b, "two repos with an identical relative path must not collide");
    }

    #[test]
    fn unique_ids_do_not_collide() {
        let ids: std::collections::HashSet<_> = (0..1000).map(|_| unique_id("l")).collect();
        assert_eq!(ids.len(), 1000);
    }
}
