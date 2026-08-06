//! Permission *before* the edit.
//!
//! [`check`](crate::check) is post-hoc: it diffs the working tree against the
//! index and reports edits already made. That is the right shape for a
//! pre-commit hook and the wrong shape for a coding agent, which can be told a
//! keystroke earlier — "may I touch this at all?" — and then go and negotiate
//! for it instead of being told off at commit time.
//!
//! Almost nothing here is new logic. `Store::conflicts_for` already walks both
//! directions of containment (`lease.rs`), so calling it on a *file* symbol
//! covers that file's whole subtree **and** its enclosing service in one query:
//! a lease on a method three levels down and a lease on the surrounding crate
//! both surface. That is the same containment guarantee `acquire` gives, which
//! is exactly what you want — a guard that disagreed with the acquire path
//! would send agents to ask for symbols they were then refused.

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::ids;
use crate::model::*;
use crate::store::Store;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GuardVerdict {
    /// A lease you hold covers it.
    Allowed,
    /// Nobody holds it, and neither do you. Legal, but nothing is protecting
    /// you from someone arriving halfway through.
    Warn,
    /// Somebody else holds it. `conflicts` says who, and for how long.
    Denied,
}

impl GuardVerdict {
    pub fn as_str(self) -> &'static str {
        match self {
            GuardVerdict::Allowed => "allowed",
            GuardVerdict::Warn => "warn",
            GuardVerdict::Denied => "denied",
        }
    }

    pub fn parse(s: &str) -> Option<GuardVerdict> {
        match s {
            "allowed" => Some(GuardVerdict::Allowed),
            "warn" => Some(GuardVerdict::Warn),
            "denied" => Some(GuardVerdict::Denied),
            _ => None,
        }
    }

    /// A warning is not a "no". Exit codes and hook decisions both key off
    /// this, so the distinction lives in one place.
    pub fn is_blocking(self) -> bool {
        self == GuardVerdict::Denied
    }
}

/// Machine-actionable remediation: what to do about a verdict, phrased twice.
/// A model reads `tool`, a shell reads `command`, and a human reads either.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GuardSuggestion {
    /// `acquire` | `request-transfer` | `wait` | `narrow`
    pub action: String,
    pub symbol: String,
    pub holder: Option<String>,
    pub seconds_until_free: Option<i64>,
    /// Ready to run, e.g. `atlas request lease "src/pay.ts:charge" --reason "..."`.
    pub command: String,
    /// The MCP tool that does the same thing, for an agent that has one.
    pub tool: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GuardReport {
    pub agent: String,
    pub repo_id: String,
    pub path: String,
    /// The symbol the verdict is about: the file, or `symbol` when narrowed.
    pub anchor: Option<String>,
    pub anchor_handle: Option<String>,
    pub verdict: GuardVerdict,
    /// Empty unless `Denied`.
    pub conflicts: Vec<Conflict>,
    /// Set when `Allowed`: the lease that covers this, and which symbol in the
    /// containment chain it actually sits on.
    pub lease_id: Option<String>,
    pub via: Option<String>,
    /// Your own leases *inside* this path. Holding one method does not entitle
    /// you to rewrite the file around it, so this does not make the verdict
    /// `Allowed` — but "nobody holds this" would be a lie when you hold part
    /// of it, and an agent that has just been handed a symbol needs to be told
    /// which part of the file is actually protected.
    pub yours_within: Vec<String>,
    /// The path is not indexed at all — a brand new file, or one never
    /// scanned. Nothing could possibly be leased, so this is `Allowed`, the
    /// same call `check` makes for an unindexed change.
    pub unindexed: bool,
    pub suggestions: Vec<GuardSuggestion>,
    /// One line, ready to hand to a model or print in a hook.
    pub summary: String,
}

impl GuardReport {
    pub fn blocking(&self) -> bool {
        self.verdict.is_blocking()
    }
}

/// Below this, waiting it out beats negotiating for it.
const WAIT_INSTEAD_SECS: i64 = 30;

impl Store {
    /// May `agent` edit `path` — optionally narrowed to `symbol` — right now?
    ///
    /// Deliberately **file-granular by default**, because the tool call being
    /// guarded (`Edit`, `Write`) names a file and not a symbol. When that is
    /// too coarse — alice holds `charge()`, bob wants `refund()` in the same
    /// file — the denial names the exact blocking symbol and carries a
    /// `narrow` suggestion, so re-asking with `symbol` is one step away and
    /// passes. Defaulting to the safe answer with a one-step path to the
    /// precise one is the honest trade here.
    pub fn guard_edit(
        &self,
        repo_id: &str,
        agent: &str,
        path: &str,
        symbol: Option<&str>,
    ) -> Result<GuardReport> {
        let path = path.replace('\\', "/");
        let anchor: Option<Symbol> = match symbol {
            Some(q) => Some(self.resolve(q)?),
            None => self.symbol(&ids::file_symbol_id(repo_id, &path))?,
        };

        let Some(anchor) = anchor else {
            return Ok(GuardReport {
                agent: agent.to_string(),
                repo_id: repo_id.to_string(),
                summary: format!("{path} is not indexed — nothing to lease yet"),
                path,
                anchor: None,
                anchor_handle: None,
                verdict: GuardVerdict::Allowed,
                conflicts: Vec::new(),
                lease_id: None,
                via: None,
                yours_within: Vec::new(),
                unindexed: true,
                suggestions: Vec::new(),
            });
        };

        let handle = anchor.handle();
        let conflicts = self.conflicts_for(&anchor.id, agent)?;

        let mut report = GuardReport {
            agent: agent.to_string(),
            repo_id: repo_id.to_string(),
            path,
            anchor: Some(anchor.id.clone()),
            anchor_handle: Some(handle.clone()),
            verdict: GuardVerdict::Denied,
            conflicts,
            lease_id: None,
            via: None,
            yours_within: Vec::new(),
            unindexed: false,
            suggestions: Vec::new(),
            summary: String::new(),
        };

        if !report.conflicts.is_empty() {
            report.suggestions = deny_suggestions(&report.conflicts, symbol.is_none());
            report.summary = deny_summary(&handle, &report.conflicts);
            return Ok(report);
        }

        // Nobody else is in the way. Is it ours? Walk outwards: holding the
        // class covers the method, and holding the service covers the file.
        let held = self.active_leases(Some(agent))?;
        let chain = std::iter::once(anchor.clone())
            .chain(self.ancestors(&anchor.id)?)
            .collect::<Vec<_>>();
        if let Some((sym, lease)) = chain
            .iter()
            .find_map(|s| held.iter().find(|l| l.symbol_id == s.id).map(|l| (s, l)))
        {
            report.verdict = GuardVerdict::Allowed;
            report.lease_id = Some(lease.id.clone());
            report.via = Some(sym.handle());
            report.summary = if sym.id == anchor.id {
                format!("{agent} holds {handle}")
            } else {
                format!("{agent} holds {}, which covers {handle}", sym.handle())
            };
            return Ok(report);
        }

        // Nothing above us is ours either. Before calling it unleased, check
        // whether we hold something *inside* it: an agent that was just handed
        // one method should not be told nobody holds the file it lives in.
        let inside = self.descendants(&anchor.id)?;
        report.yours_within = inside
            .iter()
            .filter(|s| held.iter().any(|l| l.symbol_id == s.id))
            .map(|s| s.handle())
            .collect();

        report.verdict = GuardVerdict::Warn;
        report.summary = if report.yours_within.is_empty() {
            format!("nobody holds {handle}; {agent} is editing it unleased")
        } else {
            format!(
                "{agent} holds {} inside {handle}, but not {handle} itself — the rest of it is unleased",
                report.yours_within.join(", ")
            )
        };
        report.suggestions = vec![GuardSuggestion {
            action: "acquire".to_string(),
            symbol: handle.clone(),
            holder: None,
            seconds_until_free: None,
            command: format!("atlas lease acquire {}", quote(&handle)),
            tool: "claim_symbol".to_string(),
        }];
        Ok(report)
    }
}

fn deny_suggestions(conflicts: &[Conflict], file_wide: bool) -> Vec<GuardSuggestion> {
    let mut out: Vec<GuardSuggestion> = conflicts
        .iter()
        .map(|c| {
            // Asking costs a round trip and the holder's attention. If the
            // lease lapses within seconds, waiting is simply cheaper.
            if c.seconds_until_free <= WAIT_INSTEAD_SECS {
                GuardSuggestion {
                    action: "wait".to_string(),
                    symbol: c.blocking_symbol.clone(),
                    holder: Some(c.holder.clone()),
                    seconds_until_free: Some(c.seconds_until_free),
                    command: format!(
                        "atlas lease acquire {} --wait {}",
                        quote(&c.blocking_symbol),
                        c.seconds_until_free.max(1) + 5
                    ),
                    tool: "claim_symbol".to_string(),
                }
            } else {
                GuardSuggestion {
                    action: "request-transfer".to_string(),
                    symbol: c.blocking_symbol.clone(),
                    holder: Some(c.holder.clone()),
                    seconds_until_free: Some(c.seconds_until_free),
                    command: format!(
                        "atlas request lease {} --reason \"...\"",
                        quote(&c.blocking_symbol)
                    ),
                    tool: "ask".to_string(),
                }
            }
        })
        .collect();

    // Narrowing only helps when the blocker sits *inside* what was asked
    // about — then there is other code in the same file to edit instead. If
    // somebody holds the file itself (or the service around it), there is
    // nothing narrower to retreat to, and suggesting it would send the model
    // round a loop that cannot succeed.
    if file_wide {
        if let Some(c) = conflicts
            .iter()
            .find(|c| c.relation == ConflictRelation::Descendant)
        {
            out.push(GuardSuggestion {
                action: "narrow".to_string(),
                symbol: c.blocking_symbol.clone(),
                holder: Some(c.holder.clone()),
                seconds_until_free: Some(c.seconds_until_free),
                command: format!("atlas guard <path> --symbol {}", quote(&c.blocking_symbol)),
                tool: "check_edit".to_string(),
            });
        }
    }
    out
}

fn deny_summary(handle: &str, conflicts: &[Conflict]) -> String {
    let c = &conflicts[0];
    let rest = match conflicts.len() {
        1 => String::new(),
        n => format!(" (and {} other lease(s))", n - 1),
    };
    let task = match &c.task {
        Some(t) => format!(" for task {t}"),
        None => String::new(),
    };
    format!(
        "{handle} is held by {}{task} for another {}{rest}",
        c.holder,
        human_secs(c.seconds_until_free),
    )
}

fn human_secs(secs: i64) -> String {
    if secs < 60 {
        format!("{}s", secs.max(0))
    } else {
        format!("{}m {}s", secs / 60, secs % 60)
    }
}

/// Handles contain `:` and can contain spaces; quote unless it is plainly safe.
fn quote(handle: &str) -> String {
    if handle
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "._/:-".contains(c))
    {
        handle.to_string()
    } else {
        format!("\"{}\"", handle.replace('"', "\\\""))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::DEFAULT_REPO_ID;
    use crate::lease::AcquireOptions;

    fn fixture() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("src/pay.ts"),
            "export class Payments {\n\
             \x20 charge(x: number) { return x; }\n\
             \x20 refund(x: number) { return x; }\n\
             }\n\
             export function audit() { return 1; }\n",
        )
        .unwrap();
        let mut store = Store::init(dir.path()).unwrap();
        crate::scan::scan(&mut store, DEFAULT_REPO_ID, dir.path(), &[], false).unwrap();
        store.register_agent("alice", "claude-code").unwrap();
        store.register_agent("bob", "cursor").unwrap();
        (dir, store)
    }

    fn take(store: &mut Store, agent: &str, symbol: &str) {
        let (_, outcome) = store
            .acquire_ref(symbol, agent, &AcquireOptions::default())
            .unwrap();
        assert!(
            matches!(outcome, AcquireOutcome::Granted { .. }),
            "fixture could not lease {symbol}: {outcome:?}"
        );
    }

    fn guard(store: &Store, agent: &str, symbol: Option<&str>) -> GuardReport {
        store
            .guard_edit(DEFAULT_REPO_ID, agent, "src/pay.ts", symbol)
            .unwrap()
    }

    #[test]
    fn your_own_lease_allows_the_edit() {
        let (_d, mut store) = fixture();
        take(&mut store, "alice", "src/pay.ts");

        let r = guard(&store, "alice", None);
        assert_eq!(r.verdict, GuardVerdict::Allowed);
        assert!(r.lease_id.is_some());
        assert_eq!(r.via.as_deref(), Some("src/pay.ts"));
    }

    #[test]
    fn a_lease_on_an_enclosing_scope_covers_what_is_inside_it() {
        let (_d, mut store) = fixture();
        take(&mut store, "alice", "src/pay.ts:Payments");

        let r = guard(&store, "alice", Some("src/pay.ts:Payments.charge"));
        assert_eq!(r.verdict, GuardVerdict::Allowed);
        assert_eq!(
            r.via.as_deref(),
            Some("src/pay.ts:Payments"),
            "the report must name which lease actually covers the edit"
        );
    }

    #[test]
    fn another_agents_lease_denies_the_edit() {
        let (_d, mut store) = fixture();
        take(&mut store, "alice", "src/pay.ts:Payments.charge");

        let r = guard(&store, "bob", None);
        assert_eq!(r.verdict, GuardVerdict::Denied);
        assert!(r.blocking());
        assert_eq!(r.conflicts[0].holder, "alice");
        assert!(r.summary.contains("alice"), "summary: {}", r.summary);
        assert_eq!(r.suggestions[0].action, "request-transfer");
        assert!(r.suggestions[0].command.contains("request lease"));
    }

    #[test]
    fn a_lease_deep_inside_the_file_still_blocks_the_whole_file() {
        let (_d, mut store) = fixture();
        take(&mut store, "alice", "src/pay.ts:Payments.charge");

        // Guarding the *file* must see a lease two levels down, or an agent
        // could rewrite the file wholesale around someone else's method.
        let r = guard(&store, "bob", None);
        assert_eq!(r.verdict, GuardVerdict::Denied);
        assert_eq!(r.conflicts[0].relation, ConflictRelation::Descendant);
    }

    #[test]
    fn narrowing_turns_a_file_denial_into_an_allowed_edit() {
        let (_d, mut store) = fixture();
        take(&mut store, "alice", "src/pay.ts:Payments.charge");
        take(&mut store, "bob", "src/pay.ts:Payments.refund");

        let file = guard(&store, "bob", None);
        assert_eq!(file.verdict, GuardVerdict::Denied);
        assert!(
            file.suggestions.iter().any(|s| s.action == "narrow"),
            "a file-wide denial has to offer the precise question"
        );

        let narrowed = guard(&store, "bob", Some("src/pay.ts:Payments.refund"));
        assert_eq!(narrowed.verdict, GuardVerdict::Allowed);
    }

    #[test]
    fn a_lease_on_the_whole_file_offers_no_narrowing() {
        let (_d, mut store) = fixture();
        take(&mut store, "alice", "src/pay.ts");

        let r = guard(&store, "bob", None);
        assert_eq!(r.verdict, GuardVerdict::Denied);
        assert!(
            !r.suggestions.iter().any(|s| s.action == "narrow"),
            "there is nothing narrower to retreat to, and saying otherwise sends \
             the model round a loop that cannot succeed: {:?}",
            r.suggestions.iter().map(|s| &s.action).collect::<Vec<_>>()
        );
    }

    #[test]
    fn narrowing_offers_no_further_narrowing() {
        let (_d, mut store) = fixture();
        take(&mut store, "alice", "src/pay.ts:Payments.charge");

        let r = guard(&store, "bob", Some("src/pay.ts:Payments.charge"));
        assert_eq!(r.verdict, GuardVerdict::Denied);
        assert!(!r.suggestions.iter().any(|s| s.action == "narrow"));
    }

    #[test]
    fn holding_a_method_does_not_entitle_you_to_the_file_but_is_still_said_out_loud() {
        let (_d, mut store) = fixture();
        take(&mut store, "bob", "src/pay.ts:Payments.charge");

        let r = guard(&store, "bob", None);
        assert_eq!(
            r.verdict,
            GuardVerdict::Warn,
            "one method is not the whole file"
        );
        assert_eq!(r.yours_within, vec!["src/pay.ts:Payments.charge"]);
        assert!(
            !r.summary.contains("nobody holds"),
            "telling someone nobody holds a file they hold part of is simply false: {}",
            r.summary
        );
        assert!(r.summary.contains("Payments.charge"), "{}", r.summary);
    }

    #[test]
    fn an_unleased_file_warns_but_does_not_block() {
        let (_d, store) = fixture();
        let r = guard(&store, "bob", None);
        assert_eq!(r.verdict, GuardVerdict::Warn);
        assert!(!r.blocking(), "a warning is not a no");
        assert_eq!(r.suggestions[0].action, "acquire");
        assert!(r.conflicts.is_empty());
    }

    #[test]
    fn an_unindexed_path_is_allowed() {
        let (_d, store) = fixture();
        let r = store
            .guard_edit(DEFAULT_REPO_ID, "bob", "src/brand-new.ts", None)
            .unwrap();
        assert_eq!(r.verdict, GuardVerdict::Allowed);
        assert!(r.unindexed, "nothing could possibly be leased yet");
        assert!(r.anchor.is_none());
    }

    #[test]
    fn an_expired_lease_stops_denying() {
        let (_d, mut store) = fixture();
        store
            .acquire_ref(
                "src/pay.ts:Payments.charge",
                "alice",
                &AcquireOptions {
                    ttl_secs: 1,
                    ..Default::default()
                },
            )
            .unwrap();
        store
            .conn()
            .execute(
                "UPDATE leases SET expires_at = ?1 WHERE state = 'active'",
                rusqlite::params![ids::now_ms() - 1_000],
            )
            .unwrap();

        let r = guard(&store, "bob", None);
        assert_eq!(
            r.verdict,
            GuardVerdict::Warn,
            "a lapsed lease blocks nobody, even before the sweep retires it"
        );
    }

    #[test]
    fn an_about_to_lapse_lease_suggests_waiting_rather_than_asking() {
        let (_d, mut store) = fixture();
        take(&mut store, "alice", "src/pay.ts:Payments.charge");
        store
            .conn()
            .execute(
                "UPDATE leases SET expires_at = ?1 WHERE state = 'active'",
                rusqlite::params![ids::now_ms() + 5_000],
            )
            .unwrap();

        let r = guard(&store, "bob", None);
        assert_eq!(r.verdict, GuardVerdict::Denied);
        assert_eq!(
            r.suggestions[0].action, "wait",
            "five seconds of waiting beats interrupting a teammate"
        );
    }

    #[test]
    fn the_guard_and_the_acquire_path_agree() {
        let (_d, mut store) = fixture();
        take(&mut store, "alice", "src/pay.ts:Payments.charge");

        // Whatever the guard refuses, `acquire` must also refuse — a guard
        // that sent agents to ask for symbols they were then denied would be
        // worse than no guard at all.
        let r = guard(&store, "bob", None);
        let (_, outcome) = store
            .acquire_ref("src/pay.ts", "bob", &AcquireOptions::default())
            .unwrap();
        assert_eq!(r.verdict, GuardVerdict::Denied);
        assert!(matches!(outcome, AcquireOutcome::Denied { .. }));
    }
}
