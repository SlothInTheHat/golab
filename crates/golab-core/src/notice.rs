//! What a human needs to be told.
//!
//! The runtime already produces every fact here. `notify.rs` works out that a
//! routed handler changed and who depends on it; `protocol.rs` carries reviews
//! and lease requests; the event log records the rest. All of it is addressed
//! to *agents* — an `api-change` request lands in an agent's inbox, and a
//! person watching the workspace never learns it happened.
//!
//! This is the missing half: the same facts, ranked and phrased for whoever is
//! watching. It is a **query over `requests` and `events`**, not a second
//! store of counters — the same doctrine `throughput` follows, and for the
//! same reason: a separate tally can disagree with what actually happened,
//! and a derived one cannot.
//!
//! # Contextual, not generic
//!
//! "Payment API changed" is a log line. "Payment API changed — `web` depends
//! on this, 2 follow-up tasks opened" is a notification, because it tells the
//! reader whether they have to do anything. So each notice carries the
//! consequence alongside the fact, assembled from the impact graph and from
//! the tasks the change already opened.

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::ids;
use crate::model::*;
use crate::protocol::Direction;
use crate::store::Store;

/// How far back a notice can be and still be worth showing.
const WINDOW_MS: i64 = 2 * 60 * 60 * 1000;
const DEFAULT_LIMIT: usize = 25;
/// How many dependants to name before saying "and N more".
const MAX_DEPENDANTS: usize = 3;

/// How loudly to say it. Drives colour, not behaviour.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    /// Something happened; nobody has to move.
    Info,
    /// Somebody probably has to look.
    Warn,
    /// Work is stopped until somebody acts.
    Blocking,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Notice {
    /// Stable enough to deduplicate against on the client: `req:<id>` or
    /// `evt:<id>`.
    pub id: String,
    /// `api-change`, `review`, `lease-transfer`, `dependency`, `blocked`,
    /// `contention`, `question`.
    pub kind: String,
    pub severity: Severity,
    /// One line, already phrased for a person.
    pub title: String,
    /// Why it matters to them — the dependants, the follow-ups, the holder.
    pub detail: Option<String>,
    /// Who caused it.
    pub actor: Option<String>,
    /// Who should act, if anyone in particular. `None` is everybody's problem.
    pub audience: Option<String>,
    pub symbol_handle: Option<String>,
    pub task: Option<String>,
    /// The architecture node this lands on, so clicking a notice can select
    /// the box it happened in.
    pub node: Option<String>,
    pub ts: i64,
    /// Set when the notice *is* a request somebody can answer.
    pub request: Option<String>,
}

impl Store {
    /// Everything worth telling a human, newest first.
    ///
    /// `audience` of `None` is the dashboard's view — the whole workspace.
    /// Naming an agent narrows it to what is addressed to them or broadcast,
    /// which is what a per-person panel wants.
    pub fn notifications(&self, audience: Option<&str>, limit: usize) -> Result<Vec<Notice>> {
        let limit = if limit == 0 { DEFAULT_LIMIT } else { limit };
        let now = ids::now_ms();
        let cutoff = now - WINDOW_MS;
        let mut out: Vec<Notice> = Vec::new();

        // ------------------------------------------------ open negotiations
        for r in self.requests(None, Direction::All, true)? {
            if let Some(who) = audience {
                // Broadcast requests are everybody's; addressed ones are not.
                if r.to.as_deref().is_some_and(|t| t != who) && r.from != who {
                    continue;
                }
            }
            out.push(self.notice_for_request(&r)?);
        }

        // ------------------------------------------------ high-signal events
        for e in self.recent_events(400)? {
            if e.ts < cutoff {
                continue;
            }
            let Some(n) = self.notice_for_event(&e)? else {
                continue;
            };
            if let Some(who) = audience {
                if n.audience.as_deref().is_some_and(|a| a != who) {
                    continue;
                }
            }
            out.push(n);
        }

        out.sort_by_key(|n| std::cmp::Reverse(n.ts));
        out.truncate(limit);
        Ok(out)
    }

    fn notice_for_request(&self, r: &Request) -> Result<Notice> {
        let (severity, title, detail) = match r.kind.as_str() {
            request_kind::API_CHANGE => {
                // The subject is already a sentence ("X changed"); the handle
                // is a bare symbol. Only one of them needs the verb.
                let title = match &r.resource_handle {
                    Some(h) => format!("{h} changed"),
                    None => r.subject.clone(),
                };
                let mut detail = self.dependants_phrase(r.resource_symbol.as_deref())?;
                // `notify.rs` opens follow-up tasks for a goal-linked change;
                // saying how many turns "something changed" into "and it is
                // already being dealt with".
                let followups = r.body["follow_up_tasks"]
                    .as_array()
                    .map(|a| a.len())
                    .unwrap_or(0);
                if followups > 0 {
                    detail = Some(format!(
                        "{} {} follow-up task(s) opened.",
                        detail.unwrap_or_default(),
                        followups
                    ));
                }
                (Severity::Warn, title, detail.map(|d| d.trim().to_string()))
            }
            request_kind::REVIEW => (
                Severity::Blocking,
                format!("Review sent back: {}", r.subject),
                r.body["reason"].as_str().map(|s| s.to_string()),
            ),
            request_kind::LEASE_TRANSFER => {
                let what = r.resource_handle.as_deref().unwrap_or(&r.subject);
                (
                    Severity::Blocking,
                    format!("{} wants {}", r.from, what),
                    Some(format!(
                        "Held by {}. Accepting hands it over atomically.",
                        r.to.as_deref().unwrap_or("nobody")
                    )),
                )
            }
            request_kind::DEPENDENCY => (
                Severity::Warn,
                format!("{} is blocked on {}", r.from, r.resource_task.as_deref().unwrap_or("a task")),
                Some(r.subject.clone()),
            ),
            request_kind::INTERFACE => (
                Severity::Warn,
                format!("{} needs an interface: {}", r.from, r.subject),
                None,
            ),
            _ => (Severity::Info, r.subject.clone(), None),
        };

        Ok(Notice {
            id: format!("req:{}", r.id),
            kind: r.kind.clone(),
            severity,
            title,
            detail,
            actor: Some(r.from.clone()),
            audience: r.to.clone(),
            symbol_handle: r.resource_handle.clone(),
            task: r.task.clone(),
            node: self.node_for_symbol(r.resource_symbol.as_deref())?,
            ts: r.created_at,
            request: Some(r.id.clone()),
        })
    }

    /// Events worth interrupting somebody for.
    ///
    /// Deliberately a short list. Every acquire and release goes on the
    /// timeline; a notification that fires on all of them is one nobody reads.
    fn notice_for_event(&self, e: &Event) -> Result<Option<Notice>> {
        let (kind, severity, title, detail) = match e.kind.as_str() {
            "task.unblocked" => (
                "unblocked",
                Severity::Info,
                format!(
                    "{} can start",
                    e.detail["title"].as_str().unwrap_or(e.task.as_deref().unwrap_or("A task"))
                ),
                None,
            ),
            "task.blocked" | "task.failed" => (
                "blocked",
                Severity::Blocking,
                format!("{} is stuck", e.task.as_deref().unwrap_or("A task")),
                e.detail["reason"].as_str().map(|s| s.to_string()),
            ),
            "review.submitted" => (
                "review",
                Severity::Warn,
                format!(
                    "{} submitted {} for review",
                    e.agent.as_deref().unwrap_or("someone"),
                    e.task.as_deref().unwrap_or("work")
                ),
                None,
            ),
            "goal.completed" => (
                "goal",
                Severity::Info,
                format!("Goal done: {}", e.detail["title"].as_str().unwrap_or("")),
                None,
            ),
            "lease.denied" => {
                // Contention is the one thing a human can act on that nobody
                // else will raise: two agents reaching for the same code means
                // the work is carved up wrong.
                let holder = e.detail["conflicts"][0]["holder"].as_str();
                (
                    "contention",
                    Severity::Warn,
                    format!(
                        "{} was refused {}",
                        e.agent.as_deref().unwrap_or("someone"),
                        e.symbol_handle.as_deref().unwrap_or("a symbol")
                    ),
                    holder.map(|h| format!("{h} is holding it.")),
                )
            }
            _ => return Ok(None),
        };

        Ok(Some(Notice {
            id: format!("evt:{}", e.id),
            kind: kind.to_string(),
            severity,
            title,
            detail,
            actor: e.agent.clone(),
            audience: None,
            symbol_handle: e.symbol_handle.clone(),
            task: e.task.clone(),
            node: self.node_for_handle(e.symbol_handle.as_deref())?,
            ts: e.ts,
            request: None,
        }))
    }

    /// "`web` and `mobile` depend on this." The clause that turns a fact into
    /// a reason to care.
    fn dependants_phrase(&self, symbol_id: Option<&str>) -> Result<Option<String>> {
        let Some(id) = symbol_id else {
            return Ok(None);
        };
        // Whatever service the changed symbol itself lives in is not a
        // dependant of it. "payments-api depends on payments-api" is noise
        // dressed up as a consequence.
        let own = self.owning_service(id)?;

        let mut services: Vec<String> = Vec::new();
        for node in self.impact(id, 2)? {
            // The service a dependant lives in is what a person recognises —
            // "authenticate() calls this" means less than "auth depends on it".
            let Some(name) = self.owning_service(&node.symbol.id)? else {
                continue;
            };
            if Some(&name) != own.as_ref() && !services.contains(&name) {
                services.push(name);
            }
        }
        if !services.is_empty() {
            let shown: Vec<String> = services.iter().take(MAX_DEPENDANTS).cloned().collect();
            let more = services.len().saturating_sub(shown.len());
            return Ok(Some(if more > 0 {
                format!("{} and {} more depend on this.", shown.join(", "), more)
            } else if shown.len() == 1 {
                format!("{} depends on this.", shown[0])
            } else {
                format!("{} depend on this.", shown.join(", "))
            }));
        }

        // Everything affected lives in the same service. That is still a
        // consequence worth stating — it is just a smaller one, so name the
        // callers instead of the service they are all sitting in.
        let mut callers: Vec<String> = Vec::new();
        for node in self.impact(id, 2)? {
            if node.symbol.id != id && !callers.contains(&node.symbol.name) {
                callers.push(node.symbol.name.clone());
            }
        }
        if callers.is_empty() {
            return Ok(None);
        }
        let shown: Vec<String> = callers.iter().take(MAX_DEPENDANTS).cloned().collect();
        let more = callers.len().saturating_sub(shown.len());
        Ok(Some(if more > 0 {
            format!("{}() and {} more call this.", shown.join("(), "), more)
        } else if shown.len() == 1 {
            format!("{}() calls this.", shown[0])
        } else {
            format!("{}() call this.", shown.join("(), "))
        }))
    }

    fn owning_service(&self, symbol_id: &str) -> Result<Option<String>> {
        Ok(self
            .ancestors(symbol_id)?
            .into_iter()
            .find(|a| a.kind == SymbolKind::Service)
            .map(|a| a.name))
    }

    /// Which architecture box a symbol sits in, so clicking a notice can
    /// select it on the picture.
    fn node_for_symbol(&self, symbol_id: Option<&str>) -> Result<Option<String>> {
        let Some(id) = symbol_id else {
            return Ok(None);
        };
        for anc in self.ancestors(id)? {
            if anc.kind == SymbolKind::Service {
                return Ok(Some(anc.id));
            }
        }
        // No service above it: the file is the most specific box there is.
        Ok(self
            .symbol(id)?
            .map(|s| ids::file_symbol_id(&s.repo_id, &s.path)))
    }

    fn node_for_handle(&self, handle: Option<&str>) -> Result<Option<String>> {
        let Some(h) = handle else {
            return Ok(None);
        };
        match self.resolve(h) {
            Ok(sym) => self.node_for_symbol(Some(&sym.id)),
            // An unresolvable handle is not an error here — a notice about a
            // symbol that has since been renamed away still has a story.
            Err(_) => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::DEFAULT_REPO_ID;
    use crate::lease::AcquireOptions;
    use crate::protocol::NewRequest;

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
            "export function registerRoutes(app) { app.get('/pay', charge); }\n\
             export function charge(req) { return 1; }\n",
        );
        w("web/package.json", r#"{"name":"web","version":"1.0.0"}"#);
        w(
            "web/src/app.ts",
            "import { charge } from '../../api/src/routes';\n\
             export function checkout() { return charge(1); }\n",
        );
        let mut store = Store::init(dir.path()).unwrap();
        crate::scan::scan(&mut store, DEFAULT_REPO_ID, dir.path(), &[], false).unwrap();
        store.register_agent("alice", "cursor").unwrap();
        store.register_agent("bob", "claude-code").unwrap();
        (dir, store)
    }

    #[test]
    fn an_api_change_says_who_depends_on_it() {
        let (_d, mut store) = fixture();
        let charge = store.resolve("charge").unwrap();
        store
            .open_request(&NewRequest {
                to: Some("bob".to_string()),
                resource_symbol: Some(charge.id.clone()),
                ..NewRequest::new(request_kind::API_CHANGE, "golab", "charge changed")
            })
            .unwrap();

        let n = store.notifications(None, 10).unwrap();
        let api = n.iter().find(|n| n.kind == request_kind::API_CHANGE).unwrap();
        assert!(api.title.contains("charge"), "{}", api.title);
        assert!(
            api.detail.as_deref().unwrap_or("").contains("web"),
            "a fact without a consequence is a log line: {:?}",
            api.detail
        );
        assert_eq!(api.severity, Severity::Warn);
        assert!(
            api.node.is_some(),
            "clicking a notice has to be able to select the box it happened in"
        );
    }

    /// The bug this pins: `notify.rs` used to open its `api-change` request
    /// with the symbol only in the subject line and in `body`, leaving
    /// `resource_symbol` empty. Every consequence a reader wants — who else
    /// depends on it, which box on the picture it landed in — is derived from
    /// that field, so the notice came out as a sentence with nothing behind
    /// it, and the title read "X changed changed".
    #[test]
    fn the_runtimes_own_api_change_notice_carries_its_consequences() {
        let (dir, mut store) = fixture();
        store
            .acquire_ref("checkout", "bob", &AcquireOptions::default())
            .unwrap();

        // Change the routed handler and rescan it the way the watcher does.
        let routes = dir.path().join("api/src/routes.ts");
        std::fs::write(
            &routes,
            "export function registerRoutes(app) { app.get('/pay', charge); }\n\
             export function charge(req, idempotencyKey) { return 2; }\n",
        )
        .unwrap();
        crate::notify::scan_and_notify(
            &mut store,
            DEFAULT_REPO_ID,
            dir.path(),
            &[routes],
            false,
        )
        .unwrap();

        let n = store.notifications(None, 20).unwrap();
        let api = n
            .iter()
            .find(|n| n.kind == request_kind::API_CHANGE)
            .expect("the runtime should have raised one");

        assert!(
            api.title.ends_with("changed") && !api.title.contains("changed changed"),
            "title reads badly: {}",
            api.title
        );
        assert!(
            api.symbol_handle.is_some(),
            "the notice has to say what changed"
        );
        assert_eq!(
            api.detail.as_deref(),
            Some("web depends on this."),
            "and who it breaks — naming its own service back would be noise"
        );
        assert!(
            api.node.is_some(),
            "and where on the picture to look: clicking it selects that box"
        );
    }

    /// When everything affected sits in the same service, naming the service
    /// would say "payments-api depends on payments-api". The consequence is
    /// real either way, so it drops to the callers instead of vanishing.
    #[test]
    fn an_impact_inside_one_service_names_the_callers_instead() {
        let dir = tempfile::tempdir().unwrap();
        let w = |rel: &str, body: &str| {
            let p = dir.path().join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, body).unwrap();
        };
        w("api/package.json", r#"{"name":"api","version":"1.0.0"}"#);
        w(
            "api/src/routes.ts",
            "export function registerRoutes(app) { app.get('/pay', charge); }\n\
             export function charge(req) { return 1; }\n",
        );
        w(
            "api/tests/routes.test.ts",
            "import { charge } from '../src/routes';\n\
             export function testCharge() { return charge({}); }\n",
        );
        let mut store = Store::init(dir.path()).unwrap();
        crate::scan::scan(&mut store, DEFAULT_REPO_ID, dir.path(), &[], false).unwrap();

        let charge = store.resolve("api/src/routes.ts:charge").unwrap();
        store
            .open_request(&NewRequest {
                resource_symbol: Some(charge.id.clone()),
                ..NewRequest::new(request_kind::API_CHANGE, "golab", "charge changed")
            })
            .unwrap();

        let n = store.notifications(None, 10).unwrap();
        let api = n.iter().find(|n| n.kind == request_kind::API_CHANGE).unwrap();
        let detail = api.detail.as_deref().unwrap_or("");
        assert!(
            detail.contains("testCharge") && detail.contains("call"),
            "a same-service impact still has a consequence: {detail:?}"
        );
    }

    #[test]
    fn contention_is_surfaced_because_nobody_else_will_raise_it() {
        let (_d, mut store) = fixture();
        store
            .acquire_ref("charge", "alice", &AcquireOptions::default())
            .unwrap();
        store
            .acquire_ref(
                "charge",
                "bob",
                &AcquireOptions {
                    queue: false,
                    ..Default::default()
                },
            )
            .unwrap();

        let n = store.notifications(None, 20).unwrap();
        let clash = n.iter().find(|n| n.kind == "contention").unwrap();
        assert!(clash.title.contains("bob"), "{}", clash.title);
        assert!(
            clash.detail.as_deref().unwrap_or("").contains("alice"),
            "naming the holder is what makes it actionable: {:?}",
            clash.detail
        );
    }

    fn charge_id(store: &Store) -> String {
        store.resolve("charge").unwrap().id
    }

    #[test]
    fn a_lease_request_reads_as_something_to_answer() {
        let (_d, mut store) = fixture();
        store
            .acquire_ref("charge", "alice", &AcquireOptions::default())
            .unwrap();
        store
            .request_lease_transfer(&charge_id(&store), "bob", Some("hotfix"), None, 5, None)
            .unwrap();

        let n = store.notifications(None, 20).unwrap();
        let ask = n
            .iter()
            .find(|n| n.kind == request_kind::LEASE_TRANSFER)
            .unwrap();
        assert_eq!(ask.severity, Severity::Blocking);
        assert!(ask.request.is_some(), "answerable, not just informational");
        assert_eq!(ask.audience.as_deref(), Some("alice"));
    }

    #[test]
    fn narrowing_to_one_person_drops_what_is_not_theirs() {
        let (_d, mut store) = fixture();
        store
            .acquire_ref("charge", "alice", &AcquireOptions::default())
            .unwrap();
        store
            .request_lease_transfer(&charge_id(&store), "bob", Some("hotfix"), None, 5, None)
            .unwrap();

        let hers = store.notifications(Some("alice"), 20).unwrap();
        assert!(hers.iter().any(|n| n.kind == request_kind::LEASE_TRANSFER));

        let theirs = store.notifications(Some("carol"), 20).unwrap();
        assert!(
            !theirs.iter().any(|n| n.kind == request_kind::LEASE_TRANSFER),
            "an ask addressed to alice is not carol's problem"
        );
    }

    #[test]
    fn a_quiet_workspace_produces_nothing() {
        let (_d, store) = fixture();
        assert!(
            store.notifications(None, 20).unwrap().is_empty(),
            "a notification that fires on everything is one nobody reads"
        );
    }

    #[test]
    fn the_newest_thing_is_first_and_the_list_is_capped() {
        let (_d, mut store) = fixture();
        store
            .acquire_ref("charge", "alice", &AcquireOptions::default())
            .unwrap();
        for _ in 0..8 {
            store
                .acquire_ref(
                    "charge",
                    "bob",
                    &AcquireOptions {
                        queue: false,
                        ..Default::default()
                    },
                )
                .ok();
        }

        let n = store.notifications(None, 3).unwrap();
        assert_eq!(n.len(), 3);
        assert!(n[0].ts >= n[1].ts && n[1].ts >= n[2].ts);
    }
}
