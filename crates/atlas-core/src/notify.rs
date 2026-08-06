//! Proactive change notification.
//!
//! "Agent C discovers an API change. Instead of every other agent
//! rediscovering it, the runtime broadcasts the change to interested
//! agents." Everything else in this codebase makes agents *avoid*
//! colliding; this is the one place the runtime pushes information at an
//! agent it didn't ask for.
//!
//! The diff this needs — "did this symbol's behavior change" — already
//! exists in `check.rs`, which compares the stored index against a fresh
//! re-parse before any mutation happens. Reusing it here means this module
//! adds no new comparison logic and doesn't have to intercept `scan::scan`'s
//! two-pass upsert path, which 40+ other test fixtures call directly and
//! depend on staying exactly as it is.
//!
//! Beyond notifying, a changed API symbol that belongs to a goal-linked task
//! also gets a cascading follow-up: one new task per impacted symbol not
//! already covered under that same goal, so "the auth API changed" doesn't
//! just tell the testing agent — it also opens "update the auth tests" for
//! them. A changed symbol with no goal association only ever notifies, exactly
//! as before this existed.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::Result;
use rusqlite::{params, OptionalExtension};
use serde_json::json;

use crate::check;
use crate::model::*;
use crate::protocol::NewRequest;
use crate::scan::{self, ScanStats};
use crate::store::Store;

/// How long a change notice stays open before it self-expires. It's
/// informational, not a question — nobody is required to answer it, so it
/// should not linger in an inbox forever.
const NOTICE_DEADLINE_SECS: i64 = 3600;
/// How far to follow the impact radius when deciding who to notify.
const NOTICE_DEPTH: usize = 2;

/// Scan, then notify whoever's active work depends on any API-role symbol
/// that changed.
///
/// A full-repo scan (`paths` empty) skips the notification pass entirely —
/// that's almost always an initial index build with no agents mid-task yet,
/// and diffing every symbol in the repo against itself would be pure waste
/// for a case that doesn't need it.
pub fn scan_and_notify(
    store: &mut Store,
    repo_id: &str,
    root: &Path,
    paths: &[PathBuf],
    force: bool,
) -> Result<ScanStats> {
    if paths.is_empty() {
        return scan::scan(store, repo_id, root, paths, force);
    }

    // Diff before scanning: these are still the pre-scan rows.
    let (changes, _unparsed) = check::changes(store, repo_id, root, paths)?;
    let mut candidates = Vec::new();
    for c in &changes {
        if c.change != ChangeKind::Modified {
            continue;
        }
        let Some(id) = &c.symbol_id else { continue };
        if let Some(sym) = store.symbol(id)? {
            if sym.role == Some(Role::Api) {
                candidates.push(id.clone());
            }
        }
    }

    let stats = scan::scan(store, repo_id, root, paths, force)?;

    for id in candidates {
        // Re-fetch post-scan: still the same symbol identity (ids are
        // identity-based, not content-based), just in case anything about
        // it shifted during the rebuild. Skip quietly if it's gone.
        if let Some(sym) = store.symbol(&id)? {
            notify_impact(store, &sym)?;
        }
    }

    Ok(stats)
}

/// Tell whoever holds work in `changed`'s blast radius that it moved, and —
/// if `changed` itself belongs to a goal — open a follow-up task for each
/// impacted symbol that goal doesn't already cover.
fn notify_impact(store: &mut Store, changed: &Symbol) -> Result<()> {
    let impact = store.impact(&changed.id, NOTICE_DEPTH)?;
    if impact.is_empty() {
        return Ok(());
    }

    open_followup_tasks(store, changed, &impact)?;

    let leases = store.active_leases(None)?;
    // Whoever holds the changed symbol itself is almost certainly the one
    // who just edited it — no point telling them about their own change.
    let author = leases
        .iter()
        .find(|l| l.symbol_id == changed.id)
        .map(|l| l.agent.clone());

    let impacted: HashSet<&str> = impact.iter().map(|n| n.symbol.id.as_str()).collect();
    let mut recipients: Vec<String> = leases
        .iter()
        .filter(|l| impacted.contains(l.symbol_id.as_str()))
        .filter(|l| Some(&l.agent) != author.as_ref())
        .map(|l| l.agent.clone())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    recipients.sort();

    let handle = crate::repo::qualified_handle(store, changed).unwrap_or_else(|_| changed.handle());
    for agent in recipients {
        store.open_request(&NewRequest {
            to: Some(agent),
            // The request is *about* this symbol, and saying so in the fields
            // every other request kind uses is what lets a reader work out
            // consequences: who else depends on it, and which part of the
            // architecture it lands in. Leaving them empty made the notice a
            // sentence with nothing behind it.
            resource_symbol: Some(changed.id.clone()),
            body: json!({
                "symbol": handle,
                "route": changed.route(),
                "note": "this API's behavior or signature changed",
            }),
            deadline_secs: Some(NOTICE_DEADLINE_SECS),
            ..NewRequest::new(
                request_kind::API_CHANGE,
                "atlas",
                &format!("{handle} changed"),
            )
        })?;
    }
    Ok(())
}

/// Auto-open a follow-up task for each impacted symbol not already linked to
/// `changed`'s goal — only when `changed` itself is scoped to a goal-linked
/// task. A changed symbol outside any goal opens nothing here; the pure
/// notification above still runs regardless.
fn open_followup_tasks(store: &mut Store, changed: &Symbol, impact: &[crate::graph::ImpactNode]) -> Result<()> {
    let goal_id = store
        .tasks_for_symbol(&changed.id)?
        .into_iter()
        .find_map(|task_id| store.task_goal(&task_id).ok().flatten());
    let Some(goal_id) = goal_id else {
        return Ok(());
    };

    for node in impact {
        let sym = &node.symbol;
        if goal_already_covers(store, &goal_id, &sym.id)? {
            continue;
        }
        // Only capabilities the existing, already-tested role heuristics can
        // actually support — no new file-path guessing invented here.
        let capability = match sym.role {
            Some(Role::Test) => Some(Capability::Testing),
            Some(Role::Schema) => Some(Capability::Database),
            _ => None,
        };
        let handle = crate::repo::qualified_handle(store, sym).unwrap_or_else(|_| sym.handle());
        let task = store.goal_decompose(&goal_id, &format!("Update {handle}"), 0, &[], &[sym.handle()])?;
        if let Some(cap) = capability {
            store.set_task_required_capability(&task.id, Some(cap))?;
        }
    }
    Ok(())
}

/// Has `goal_id` already got a task scoped to `symbol_id`, in any state?
/// Checked regardless of state so re-editing the same API symbol after its
/// cascade already resolved doesn't spawn a second task for the same
/// impacted symbol on every subsequent save.
fn goal_already_covers(store: &Store, goal_id: &str, symbol_id: &str) -> Result<bool> {
    Ok(store
        .conn()
        .query_row(
            "SELECT 1 FROM task_symbols ts JOIN task_goals tg ON tg.task_id = ts.task_id \
             WHERE tg.goal_id = ?1 AND ts.symbol_id = ?2",
            params![goal_id, symbol_id],
            |_| Ok(true),
        )
        .optional()?
        .unwrap_or(false))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lease::AcquireOptions;
    use crate::protocol::Direction;

    /// `getPayment` calls `record`, which is API-routed, so editing `record`
    /// should reach `getPayment`'s holder.
    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("src/api.py"),
            "@app.get(\"/payments/{id}\")\n\
             def get_payment(id):\n\
             \x20   return record(id)\n\n\
             def record(id):\n\
             \x20   return 1\n",
        )
        .unwrap();
        dir
    }

    /// `scan_and_notify`'s candidate filter is specifically "an API-role
    /// symbol changed" — so this fixture needs the *handler itself* to
    /// change, with something else in the repo calling that handler, which
    /// is the shape `impact()` can actually see (a receipt endpoint that
    /// reuses the payment lookup, say).
    fn two_endpoint_fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("src/api.py"),
            "@app.get(\"/payments/{id}\")\n\
             def get_payment(id):\n\
             \x20   return 1\n\n\
             @app.get(\"/payments/{id}/receipt\")\n\
             def get_receipt(id):\n\
             \x20   return format(get_payment(id))\n",
        )
        .unwrap();
        dir
    }

    #[test]
    fn changing_an_api_symbol_notifies_agents_in_its_impact_radius() {
        let dir = two_endpoint_fixture();
        let mut store = Store::init(dir.path()).unwrap();
        scan::scan(&mut store, crate::ids::DEFAULT_REPO_ID, dir.path(), &[], false).unwrap();

        // watcher-agent's work depends on get_payment through get_receipt.
        let receipt = store.resolve("get_receipt").unwrap();
        store
            .acquire(&receipt.id, "watcher-agent", &AcquireOptions::default())
            .unwrap();

        std::fs::write(
            dir.path().join("src/api.py"),
            "@app.get(\"/payments/{id}\")\n\
             def get_payment(id):\n\
             \x20   return 2\n\n\
             @app.get(\"/payments/{id}/receipt\")\n\
             def get_receipt(id):\n\
             \x20   return format(get_payment(id))\n",
        )
        .unwrap();
        scan_and_notify(&mut store, crate::ids::DEFAULT_REPO_ID, dir.path(), &[PathBuf::from("src/api.py")], false).unwrap();

        let inbox = store
            .requests(Some("watcher-agent"), Direction::Inbox, true)
            .unwrap();
        assert_eq!(inbox.len(), 1, "{inbox:?}");
        assert_eq!(inbox[0].kind, request_kind::API_CHANGE);
        assert_eq!(inbox[0].body["symbol"], "src/api.py:get_payment");
    }

    #[test]
    fn the_agent_who_holds_the_changed_symbols_lease_is_not_notified() {
        let dir = two_endpoint_fixture();
        let mut store = Store::init(dir.path()).unwrap();
        scan::scan(&mut store, crate::ids::DEFAULT_REPO_ID, dir.path(), &[], false).unwrap();

        // The author holds both the endpoint they're editing and its caller.
        let handler = store.resolve("get_payment").unwrap();
        let receipt = store.resolve("get_receipt").unwrap();
        store
            .acquire(&handler.id, "author", &AcquireOptions::default())
            .unwrap();
        store
            .acquire(&receipt.id, "author", &AcquireOptions::default())
            .unwrap();

        std::fs::write(
            dir.path().join("src/api.py"),
            "@app.get(\"/payments/{id}\")\n\
             def get_payment(id):\n\
             \x20   return 2\n\n\
             @app.get(\"/payments/{id}/receipt\")\n\
             def get_receipt(id):\n\
             \x20   return format(get_payment(id))\n",
        )
        .unwrap();
        scan_and_notify(&mut store, crate::ids::DEFAULT_REPO_ID, dir.path(), &[PathBuf::from("src/api.py")], false).unwrap();

        assert!(
            store
                .requests(Some("author"), Direction::Inbox, true)
                .unwrap()
                .is_empty(),
            "the author of the change should not be notified about their own edit"
        );
    }

    #[test]
    fn full_scans_do_not_trigger_notifications() {
        let dir = two_endpoint_fixture();
        let mut store = Store::init(dir.path()).unwrap();
        scan::scan(&mut store, crate::ids::DEFAULT_REPO_ID, dir.path(), &[], false).unwrap();
        let receipt = store.resolve("get_receipt").unwrap();
        store
            .acquire(&receipt.id, "watcher-agent", &AcquireOptions::default())
            .unwrap();

        std::fs::write(
            dir.path().join("src/api.py"),
            "@app.get(\"/payments/{id}\")\n\
             def get_payment(id):\n\
             \x20   return 2\n\n\
             @app.get(\"/payments/{id}/receipt\")\n\
             def get_receipt(id):\n\
             \x20   return format(get_payment(id))\n",
        )
        .unwrap();
        // Empty paths = a full scan, which intentionally skips the diff.
        scan_and_notify(&mut store, crate::ids::DEFAULT_REPO_ID, dir.path(), &[], false).unwrap();

        assert!(store
            .requests(Some("watcher-agent"), Direction::Inbox, true)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn notice_requests_expire_on_their_own() {
        let dir = fixture();
        let mut store = Store::init(dir.path()).unwrap();
        scan::scan(&mut store, crate::ids::DEFAULT_REPO_ID, dir.path(), &[], false).unwrap();
        let handler = store.resolve("get_payment").unwrap();
        store
            .acquire(&handler.id, "watcher-agent", &AcquireOptions::default())
            .unwrap();

        let record = store.resolve("record").unwrap();
        notify_impact(&mut store, &record).unwrap();
        let before = store
            .requests(Some("watcher-agent"), Direction::Inbox, true)
            .unwrap();
        assert_eq!(before.len(), 1);
        assert!(before[0].deadline_at.is_some(), "informational notices must not linger forever");
    }

    #[test]
    fn no_lease_holder_on_the_changed_symbol_means_a_broadcast() {
        let dir = fixture();
        let mut store = Store::init(dir.path()).unwrap();
        scan::scan(&mut store, crate::ids::DEFAULT_REPO_ID, dir.path(), &[], false).unwrap();

        // Nobody holds `record` itself, but someone holds its caller.
        let handler = store.resolve("get_payment").unwrap();
        store
            .acquire(&handler.id, "watcher-agent", &AcquireOptions::default())
            .unwrap();
        let record = store.resolve("record").unwrap();
        notify_impact(&mut store, &record).unwrap();

        assert_eq!(
            store
                .requests(Some("watcher-agent"), Direction::Inbox, true)
                .unwrap()
                .len(),
            1,
            "with no clear author, everyone downstream should hear about it"
        );
    }

    /// `get_payment` (API) is called by `get_receipt` (also API) and by a
    /// test — the shape that lets one change exercise both the
    /// no-capability-signal path and the `Role::Test -> Testing` mapping.
    fn goal_linked_fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::create_dir_all(dir.path().join("tests")).unwrap();
        std::fs::write(
            dir.path().join("src/api.py"),
            "@app.get(\"/payments/{id}\")\n\
             def get_payment(id):\n\
             \x20   return 1\n\n\
             @app.get(\"/payments/{id}/receipt\")\n\
             def get_receipt(id):\n\
             \x20   return format(get_payment(id))\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("tests/test_api.py"),
            "def test_get_payment():\n    return get_payment(1)\n",
        )
        .unwrap();
        dir
    }

    #[test]
    fn a_goal_linked_api_change_opens_followup_tasks_with_role_derived_capability() {
        let dir = goal_linked_fixture();
        let mut store = Store::init(dir.path()).unwrap();
        scan::scan(&mut store, crate::ids::DEFAULT_REPO_ID, dir.path(), &[], false).unwrap();

        let goal = store.add_goal("Support payments", 5, None, None).unwrap();
        let handler = store.resolve("get_payment").unwrap();
        store
            .goal_decompose(&goal.id, "wire the handler", 5, &[], &[handler.handle()])
            .unwrap();

        std::fs::write(
            dir.path().join("src/api.py"),
            "@app.get(\"/payments/{id}\")\n\
             def get_payment(id):\n\
             \x20   return 2\n\n\
             @app.get(\"/payments/{id}/receipt\")\n\
             def get_receipt(id):\n\
             \x20   return format(get_payment(id))\n",
        )
        .unwrap();
        scan_and_notify(
            &mut store,
            crate::ids::DEFAULT_REPO_ID,
            dir.path(),
            &[PathBuf::from("src/api.py")],
            false,
        )
        .unwrap();

        let tasks = store.goal_tasks(&goal.id).unwrap();
        assert_eq!(tasks.len(), 3, "{tasks:?}", );

        let test_task = tasks
            .iter()
            .find(|t| t.task.title.contains("test_get_payment"))
            .expect("a follow-up task for the test should have been opened");
        assert_eq!(test_task.task.required_capability, Some(Capability::Testing));

        let receipt_task = tasks
            .iter()
            .find(|t| t.task.title.contains("get_receipt"))
            .expect("a follow-up task for the other caller should have been opened");
        assert_eq!(receipt_task.task.required_capability, None);
    }

    #[test]
    fn a_second_edit_does_not_open_duplicate_followup_tasks() {
        let dir = goal_linked_fixture();
        let mut store = Store::init(dir.path()).unwrap();
        scan::scan(&mut store, crate::ids::DEFAULT_REPO_ID, dir.path(), &[], false).unwrap();

        let goal = store.add_goal("Support payments", 5, None, None).unwrap();
        let handler = store.resolve("get_payment").unwrap();
        store
            .goal_decompose(&goal.id, "wire the handler", 5, &[], &[handler.handle()])
            .unwrap();

        for body in [
            "@app.get(\"/payments/{id}\")\ndef get_payment(id):\n    return 2\n\n@app.get(\"/payments/{id}/receipt\")\ndef get_receipt(id):\n    return format(get_payment(id))\n",
            "@app.get(\"/payments/{id}\")\ndef get_payment(id):\n    return 3\n\n@app.get(\"/payments/{id}/receipt\")\ndef get_receipt(id):\n    return format(get_payment(id))\n",
        ] {
            std::fs::write(dir.path().join("src/api.py"), body).unwrap();
            scan_and_notify(
                &mut store,
                crate::ids::DEFAULT_REPO_ID,
                dir.path(),
                &[PathBuf::from("src/api.py")],
                false,
            )
            .unwrap();
        }

        assert_eq!(
            store.goal_tasks(&goal.id).unwrap().len(),
            3,
            "re-editing the same API symbol must not spawn a second follow-up per impacted symbol"
        );
    }

    #[test]
    fn a_changed_symbol_with_no_goal_association_opens_no_followup_tasks() {
        let dir = goal_linked_fixture();
        let mut store = Store::init(dir.path()).unwrap();
        scan::scan(&mut store, crate::ids::DEFAULT_REPO_ID, dir.path(), &[], false).unwrap();
        // No goal, no task scoping — same as every other test in this file.

        std::fs::write(
            dir.path().join("src/api.py"),
            "@app.get(\"/payments/{id}\")\ndef get_payment(id):\n    return 2\n\n@app.get(\"/payments/{id}/receipt\")\ndef get_receipt(id):\n    return format(get_payment(id))\n",
        )
        .unwrap();
        scan_and_notify(
            &mut store,
            crate::ids::DEFAULT_REPO_ID,
            dir.path(),
            &[PathBuf::from("src/api.py")],
            false,
        )
        .unwrap();

        assert!(store.goals().unwrap().is_empty());
        assert!(store.tasks().unwrap().is_empty(), "no goal means no cascade, only the existing notification");
    }
}
