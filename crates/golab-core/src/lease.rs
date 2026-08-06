//! Symbol leasing.
//!
//! A lease is ownership of a symbol that expires. Three properties matter:
//!
//! 1. **Atomic.** Acquisition runs in one IMMEDIATE transaction, so two agents
//!    racing for `processPayment()` cannot both win, even from separate
//!    processes on separate machines sharing the database.
//! 2. **Containment-aware.** Holding a class blocks its methods and vice
//!    versa — file/class/function nest, and so must their leases.
//! 3. **Self-healing.** A crashed agent stops heartbeating and its lease
//!    expires; nothing has to notice the crash for the lock to be released.

use anyhow::{anyhow, Result};
use rusqlite::{params, OptionalExtension, Row, Transaction};
use serde_json::json;

use crate::ids;
use crate::model::*;
use crate::store::{self, Store};

/// Default lease lifetime when the caller does not specify one.
pub const DEFAULT_TTL_SECS: i64 = 300;
/// A queued waiter that stops polling for this long loses its place.
pub const QUEUE_ABANDON_MS: i64 = 15_000;

const LEASE_SELECT: &str = "SELECT l.id, l.symbol_id, \
    COALESCE(CASE WHEN s.kind = 'file' THEN s.path ELSE s.path || ':' || s.fqn END, l.symbol_id), \
    l.agent, l.task, l.priority, l.ttl_secs, l.acquired_at, l.heartbeat_at, l.expires_at, \
    l.state, l.note \
    FROM leases l LEFT JOIN symbols s ON s.id = l.symbol_id";

#[derive(Debug, Clone)]
pub struct AcquireOptions {
    pub task: Option<String>,
    pub ttl_secs: i64,
    /// Higher wins. Only meaningful against other requests, never against time.
    pub priority: i64,
    pub note: Option<String>,
    /// Take the lease from a strictly lower-priority holder.
    pub preempt: bool,
    /// Record a place in line when denied, so waiting is fair and visible.
    pub queue: bool,
}

impl Default for AcquireOptions {
    fn default() -> Self {
        AcquireOptions {
            task: None,
            ttl_secs: DEFAULT_TTL_SECS,
            priority: 0,
            note: None,
            preempt: false,
            queue: true,
        }
    }
}

impl Store {
    /// Retire every lease whose TTL has run out. Safe to call at any time;
    /// this is what makes a crashed agent stop blocking its teammates.
    pub fn expire_due(&mut self) -> Result<usize> {
        self.write(expire_due_tx)
    }

    /// Try to take a lease on `symbol_id` for `agent`.
    pub fn acquire(
        &mut self,
        symbol_id: &str,
        agent: &str,
        opts: &AcquireOptions,
    ) -> Result<AcquireOutcome> {
        let symbol_id = symbol_id.to_string();
        let agent = agent.to_string();
        let opts = opts.clone();
        self.write(move |tx| acquire_tx(tx, &symbol_id, &agent, &opts))
    }

    /// Resolve a user-supplied symbol reference, then acquire it.
    pub fn acquire_ref(
        &mut self,
        query: &str,
        agent: &str,
        opts: &AcquireOptions,
    ) -> Result<(Symbol, AcquireOutcome)> {
        let symbol = self.resolve(query)?;
        let outcome = self.acquire(&symbol.id, agent, opts)?;
        Ok((symbol, outcome))
    }

    /// Release one lease. Only its owner may release it.
    pub fn release(&mut self, lease_id: &str, agent: &str) -> Result<Lease> {
        let lease_id = lease_id.to_string();
        let agent = agent.to_string();
        self.write(move |tx| {
            let lease = load_lease(tx, &lease_id)?
                .ok_or_else(|| anyhow!("no such lease: {lease_id}"))?;
            if lease.agent != agent {
                return Err(anyhow!(
                    "lease {lease_id} belongs to {}, not {agent}",
                    lease.agent
                ));
            }
            if lease.state != LeaseState::Active {
                return Err(anyhow!(
                    "lease {lease_id} is already {}",
                    lease.state.as_str()
                ));
            }
            finish_lease(tx, &lease, LeaseState::Released, "released")?;
            load_lease(tx, &lease_id)?.ok_or_else(|| anyhow!("lease vanished"))
        })
    }

    /// Hand a lease to another agent without it ever becoming free.
    ///
    /// Release-then-reacquire has a hole in the middle: any third agent (or a
    /// queued waiter) can take the symbol in the gap. A transfer closes it —
    /// one transaction, one lease id, a new owner and a fresh TTL.
    pub fn transfer_lease(&mut self, lease_id: &str, from: &str, to: &str) -> Result<Lease> {
        let lease_id = lease_id.to_string();
        let from = from.to_string();
        let to = to.to_string();
        self.write(move |tx| transfer_lease_tx(tx, &lease_id, &from, &to))
    }

    /// Release every active lease held by `agent`.
    pub fn release_all(&mut self, agent: &str) -> Result<Vec<Lease>> {
        let agent = agent.to_string();
        self.write(move |tx| {
            let held = leases_for_agent(tx, &agent)?;
            for l in &held {
                finish_lease(tx, l, LeaseState::Released, "released")?;
            }
            Ok(held)
        })
    }

    /// Refresh TTLs. With `lease_id` set, only that lease; otherwise every
    /// lease the agent holds. This is the agent saying "I am still alive".
    pub fn heartbeat(&mut self, agent: &str, lease_id: Option<&str>) -> Result<Vec<Lease>> {
        let agent = agent.to_string();
        let lease_id = lease_id.map(|s| s.to_string());
        self.write(move |tx| {
            expire_due_tx(tx)?;
            let now = ids::now_ms();
            let held = match &lease_id {
                Some(id) => load_lease(tx, id)?
                    .filter(|l| l.agent == agent && l.state == LeaseState::Active)
                    .into_iter()
                    .collect(),
                None => leases_for_agent(tx, &agent)?,
            };
            for l in &held {
                tx.execute(
                    "UPDATE leases SET heartbeat_at = ?2, expires_at = ?3 WHERE id = ?1",
                    params![l.id, now, now + l.ttl_secs * 1000],
                )?;
            }
            tx.execute(
                "UPDATE agents SET heartbeat_at = ?2 WHERE name = ?1",
                params![agent, now],
            )?;
            // Re-read so callers see the new expiry.
            let mut out = Vec::with_capacity(held.len());
            for l in &held {
                if let Some(fresh) = load_lease(tx, &l.id)? {
                    out.push(fresh);
                }
            }
            Ok(out)
        })
    }

    /// Currently held leases, optionally filtered to one agent.
    pub fn active_leases(&self, agent: Option<&str>) -> Result<Vec<Lease>> {
        let sql = match agent {
            Some(_) => format!(
                "{LEASE_SELECT} WHERE l.state = 'active' AND l.agent = ?1 ORDER BY l.acquired_at"
            ),
            None => format!("{LEASE_SELECT} WHERE l.state = 'active' ORDER BY l.acquired_at"),
        };
        let mut stmt = self.conn().prepare(&sql)?;
        let rows = match agent {
            Some(a) => stmt.query_map(params![a], row_to_lease)?,
            None => stmt.query_map([], row_to_lease)?,
        };
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    pub fn lease(&self, id: &str) -> Result<Option<Lease>> {
        Ok(self
            .conn()
            .query_row(
                &format!("{LEASE_SELECT} WHERE l.id = ?1"),
                params![id],
                row_to_lease,
            )
            .optional()?)
    }

    pub fn lease_history(&self, limit: usize) -> Result<Vec<Lease>> {
        let mut stmt = self
            .conn()
            .prepare(&format!("{LEASE_SELECT} ORDER BY l.acquired_at DESC LIMIT ?1"))?;
        let rows = stmt.query_map(params![limit as i64], row_to_lease)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Who would block `agent` from taking `symbol_id` right now. Read-only:
    /// agents can plan around contention without creating any state.
    pub fn conflicts_for(&self, symbol_id: &str, agent: &str) -> Result<Vec<Conflict>> {
        let mut related = vec![symbol_id.to_string()];
        related.extend(self.ancestors(symbol_id)?.into_iter().map(|s| s.id));
        related.extend(self.descendants(symbol_id)?.into_iter().map(|s| s.id));
        related.sort();
        related.dedup();

        let now = ids::now_ms();
        let mut out = Vec::new();
        for id in related {
            let sql = format!(
                "{LEASE_SELECT} WHERE l.symbol_id = ?1 AND l.state = 'active' AND l.agent != ?2"
            );
            let mut stmt = self.conn().prepare(&sql)?;
            let rows = stmt.query_map(params![id, agent], row_to_lease)?;
            for lease in rows {
                let lease = lease?;
                if lease.expires_at <= now {
                    continue; // stale; the next write will retire it
                }
                out.push(conflict_from(&lease, relation_of(symbol_id, &id, self)?));
            }
        }
        Ok(out)
    }

    /// Agents waiting on a symbol, best claim first.
    pub fn queue_for(&self, symbol_id: &str) -> Result<Vec<QueueEntry>> {
        let mut stmt = self.conn().prepare(
            "SELECT id, seq, symbol_id, agent, task, priority, ttl_secs, requested_at, polled_at \
             FROM lease_queue WHERE symbol_id = ?1 AND state = 'waiting' \
             ORDER BY priority DESC, seq ASC",
        )?;
        let rows = stmt.query_map(params![symbol_id], |r| {
            Ok(QueueEntry {
                id: r.get(0)?,
                seq: r.get(1)?,
                symbol_id: r.get(2)?,
                agent: r.get(3)?,
                task: r.get(4)?,
                priority: r.get(5)?,
                ttl_secs: r.get(6)?,
                requested_at: r.get(7)?,
                polled_at: r.get(8)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    pub fn waiting_count(&self) -> Result<i64> {
        Ok(self.conn().query_row(
            "SELECT COUNT(*) FROM lease_queue WHERE state = 'waiting'",
            [],
            |r| r.get(0),
        )?)
    }

    /// Give up a place in line.
    pub fn dequeue(&mut self, agent: &str, symbol_id: &str) -> Result<usize> {
        let agent = agent.to_string();
        let symbol_id = symbol_id.to_string();
        self.write(move |tx| {
            let n = tx.execute(
                "UPDATE lease_queue SET state = 'cancelled' \
                 WHERE agent = ?1 AND symbol_id = ?2 AND state = 'waiting'",
                params![agent, symbol_id],
            )?;
            Ok(n)
        })
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct QueueEntry {
    pub id: String,
    /// Monotonic arrival order. Wall-clock time is too coarse: two agents can
    /// queue inside the same millisecond and the line must still be a line.
    pub seq: i64,
    pub symbol_id: String,
    pub agent: String,
    pub task: Option<String>,
    pub priority: i64,
    pub ttl_secs: i64,
    pub requested_at: i64,
    pub polled_at: i64,
}

// ------------------------------------------------------------------ internals

fn acquire_tx(
    tx: &Transaction,
    symbol_id: &str,
    agent: &str,
    opts: &AcquireOptions,
) -> Result<AcquireOutcome> {
    expire_due_tx(tx)?;
    abandon_stale_waiters(tx)?;

    let symbol = store::tx_symbol(tx, symbol_id)?
        .ok_or_else(|| anyhow!("unknown symbol id: {symbol_id}"))?;
    let handle = symbol.handle();
    let now = ids::now_ms();

    // Already ours? Extend rather than stacking duplicate leases.
    if let Some(mine) = active_lease_on(tx, symbol_id)? {
        if mine.agent == agent {
            let expires = now + opts.ttl_secs * 1000;
            tx.execute(
                "UPDATE leases SET heartbeat_at = ?2, expires_at = ?3, ttl_secs = ?4, \
                 priority = ?5, task = COALESCE(?6, task) WHERE id = ?1",
                params![
                    mine.id,
                    now,
                    expires,
                    opts.ttl_secs,
                    opts.priority,
                    opts.task
                ],
            )?;
            let lease = load_lease(tx, &mine.id)?.ok_or_else(|| anyhow!("lease vanished"))?;
            store::emit(
                tx,
                "lease.extended",
                Some(agent),
                Some(&handle),
                lease.task.as_deref(),
                json!({ "lease": lease.id, "expires_at": lease.expires_at }),
            )?;
            return Ok(AcquireOutcome::Extended { lease });
        }
    }

    // Anyone holding this symbol, a scope containing it, or a scope inside it.
    let (conflicts, blocking_leases) = conflicts_tx(tx, symbol_id, agent)?;

    if !conflicts.is_empty() {
        if opts.preempt && conflicts.iter().all(|c| c.priority < opts.priority) {
            for lease in &blocking_leases {
                finish_lease(tx, lease, LeaseState::Preempted, "preempted")?;
                store::emit(
                    tx,
                    "lease.preempted",
                    Some(&lease.agent),
                    Some(&lease.symbol_handle),
                    lease.task.as_deref(),
                    json!({
                        "lease": lease.id,
                        "by": agent,
                        "their_priority": opts.priority,
                        "your_priority": lease.priority,
                    }),
                )?;
            }
        } else {
            if opts.queue {
                enqueue(tx, symbol_id, &handle, agent, opts, now)?;
            }
            store::emit(
                tx,
                "lease.denied",
                Some(agent),
                Some(&handle),
                opts.task.as_deref(),
                json!({ "conflicts": conflicts }),
            )?;
            return Ok(AcquireOutcome::Denied { conflicts });
        }
    }

    // Free — but respect anyone who has been waiting longer or ranks higher.
    if let Some(ahead) = waiter_ahead_of(tx, symbol_id, agent, opts, now)? {
        if opts.queue {
            enqueue(tx, symbol_id, &handle, agent, opts, now)?;
        }
        let conflicts = vec![Conflict {
            blocking_symbol: handle.clone(),
            relation: ConflictRelation::Queued,
            holder: ahead.agent.clone(),
            task: ahead.task.clone(),
            priority: ahead.priority,
            expires_at: now,
            seconds_until_free: 0,
            lease_id: ahead.id.clone(),
        }];
        store::emit(
            tx,
            "lease.denied",
            Some(agent),
            Some(&handle),
            opts.task.as_deref(),
            json!({ "conflicts": conflicts }),
        )?;
        return Ok(AcquireOutcome::Denied { conflicts });
    }

    let lease_id = ids::unique_id("lease");
    let expires = now + opts.ttl_secs * 1000;
    tx.execute(
        "INSERT INTO leases(id, symbol_id, agent, task, priority, ttl_secs, acquired_at, \
             heartbeat_at, expires_at, state, note, released_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7, ?8, 'active', ?9, NULL)",
        params![
            lease_id,
            symbol_id,
            agent,
            opts.task,
            opts.priority,
            opts.ttl_secs,
            now,
            expires,
            opts.note
        ],
    )?;
    tx.execute(
        "UPDATE lease_queue SET state = 'granted' WHERE agent = ?1 AND symbol_id = ?2 \
         AND state = 'waiting'",
        params![agent, symbol_id],
    )?;

    let lease = load_lease(tx, &lease_id)?.ok_or_else(|| anyhow!("lease vanished"))?;
    store::emit(
        tx,
        "lease.acquired",
        Some(agent),
        Some(&handle),
        lease.task.as_deref(),
        json!({
            "lease": lease.id,
            "kind": symbol.kind.as_str(),
            "ttl_secs": lease.ttl_secs,
            "expires_at": lease.expires_at,
            "priority": lease.priority,
        }),
    )?;
    Ok(AcquireOutcome::Granted { lease })
}

fn expire_due_tx(tx: &Transaction) -> Result<usize> {
    let now = ids::now_ms();
    let mut stmt = tx.prepare(&format!(
        "{LEASE_SELECT} WHERE l.state = 'active' AND l.expires_at <= ?1"
    ))?;
    let due: Vec<Lease> = stmt
        .query_map(params![now], row_to_lease)?
        .collect::<Result<Vec<_>, _>>()?;
    drop(stmt);

    for lease in &due {
        finish_lease(tx, lease, LeaseState::Expired, "ttl expired")?;
    }
    Ok(due.len())
}

/// Waiters that stopped polling are assumed dead so the queue cannot wedge.
fn abandon_stale_waiters(tx: &Transaction) -> Result<usize> {
    let cutoff = ids::now_ms() - QUEUE_ABANDON_MS;
    Ok(tx.execute(
        "UPDATE lease_queue SET state = 'abandoned' WHERE state = 'waiting' AND polled_at < ?1",
        params![cutoff],
    )?)
}

fn enqueue(
    tx: &Transaction,
    symbol_id: &str,
    handle: &str,
    agent: &str,
    opts: &AcquireOptions,
    now: i64,
) -> Result<()> {
    let existing: Option<String> = tx
        .query_row(
            "SELECT id FROM lease_queue WHERE agent = ?1 AND symbol_id = ?2 AND state = 'waiting'",
            params![agent, symbol_id],
            |r| r.get(0),
        )
        .optional()?;
    match existing {
        Some(id) => {
            // Keep the original requested_at: polling must not lose your place.
            tx.execute(
                "UPDATE lease_queue SET polled_at = ?2, priority = ?3 WHERE id = ?1",
                params![id, now, opts.priority],
            )?;
        }
        None => {
            tx.execute(
                "INSERT INTO lease_queue(id, seq, symbol_id, agent, task, priority, ttl_secs, \
                     requested_at, polled_at, state, note) \
                 VALUES (?1, (SELECT COALESCE(MAX(seq), 0) + 1 FROM lease_queue), \
                     ?2, ?3, ?4, ?5, ?6, ?7, ?7, 'waiting', ?8)",
                params![
                    ids::unique_id("q"),
                    symbol_id,
                    agent,
                    opts.task,
                    opts.priority,
                    opts.ttl_secs,
                    now,
                    opts.note
                ],
            )?;
            store::emit(
                tx,
                "lease.queued",
                Some(agent),
                Some(handle),
                opts.task.as_deref(),
                json!({ "symbol_id": symbol_id, "priority": opts.priority }),
            )?;
        }
    }
    Ok(())
}

/// The waiter with a better claim than this request, if any.
fn waiter_ahead_of(
    tx: &Transaction,
    symbol_id: &str,
    agent: &str,
    opts: &AcquireOptions,
    now: i64,
) -> Result<Option<QueueEntry>> {
    let mut stmt = tx.prepare(
        "SELECT id, seq, symbol_id, agent, task, priority, ttl_secs, requested_at, polled_at \
         FROM lease_queue WHERE symbol_id = ?1 AND state = 'waiting' AND agent != ?2 \
         ORDER BY priority DESC, seq ASC LIMIT 1",
    )?;
    let head: Option<QueueEntry> = stmt
        .query_row(params![symbol_id, agent], |r| {
            Ok(QueueEntry {
                id: r.get(0)?,
                seq: r.get(1)?,
                symbol_id: r.get(2)?,
                agent: r.get(3)?,
                task: r.get(4)?,
                priority: r.get(5)?,
                ttl_secs: r.get(6)?,
                requested_at: r.get(7)?,
                polled_at: r.get(8)?,
            })
        })
        .optional()?;

    let Some(head) = head else { return Ok(None) };
    if head.polled_at < now - QUEUE_ABANDON_MS {
        return Ok(None);
    }
    // Our own place in line. Without one we are effectively last.
    let mine: Option<(i64, i64)> = tx
        .query_row(
            "SELECT priority, seq FROM lease_queue \
             WHERE symbol_id = ?1 AND agent = ?2 AND state = 'waiting'",
            params![symbol_id, agent],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let (my_priority, my_seq) = mine.unwrap_or((opts.priority, i64::MAX));
    let ahead =
        head.priority > my_priority || (head.priority == my_priority && head.seq < my_seq);
    Ok(if ahead { Some(head) } else { None })
}

fn finish_lease(
    tx: &Transaction,
    lease: &Lease,
    state: LeaseState,
    reason: &str,
) -> Result<()> {
    tx.execute(
        "UPDATE leases SET state = ?2, released_at = ?3 WHERE id = ?1",
        params![lease.id, state.as_str(), ids::now_ms()],
    )?;
    let kind = match state {
        LeaseState::Released => "lease.released",
        LeaseState::Expired => "lease.expired",
        LeaseState::Preempted => "lease.preempted",
        LeaseState::Active => "lease.updated",
    };
    // Whoever asked for this symbol gets their answer for free — but only if
    // it actually became free. Preemption just swaps one holder for another.
    if matches!(state, LeaseState::Released | LeaseState::Expired) {
        crate::protocol::resolve_symbol_requests(tx, &lease.symbol_id, reason)?;
    }
    // Preemption emits its own richer event at the call site.
    if state != LeaseState::Preempted {
        store::emit(
            tx,
            kind,
            Some(&lease.agent),
            Some(&lease.symbol_handle),
            lease.task.as_deref(),
            json!({
                "lease": lease.id,
                "reason": reason,
                "held_ms": ids::now_ms() - lease.acquired_at,
            }),
        )?;
    }
    Ok(())
}

/// Every active lease that would block `agent` from taking `symbol_id`,
/// inside an existing transaction.
///
/// Shared by acquisition and by the scheduler: "would this collide?" must be
/// answered the same way whether an agent is asking for a lease or the
/// scheduler is deciding whether a task is safe to hand out.
/// Check every symbol in `scope` against `agent`; only if the whole scope is
/// clear, acquire all of it. All-or-nothing, so a caller never ends up
/// holding half a task's scope because the other half collided.
///
/// Shared by the scheduler's claim path and by reassignment: whichever way an
/// agent ends up owning a task, the symbols underneath it are settled the
/// same way. Returns the conflicts found (empty on success) rather than an
/// `Err`, so the caller decides what "no" means for them.
pub(crate) fn settle_scope_tx(
    tx: &Transaction,
    scope: &[String],
    agent: &str,
    opts: &AcquireOptions,
) -> Result<Vec<Conflict>> {
    let mut conflicts = Vec::new();
    for sym in scope {
        let (sym_conflicts, _) = conflicts_tx(tx, sym, agent)?;
        if sym_conflicts.is_empty() {
            continue;
        }
        // Mirror acquire_tx's own preemption rule here: a conflict is only a
        // real blocker if preemption won't clear it, otherwise the all-or-
        // nothing check below would refuse a scope that acquire_in_tx (which
        // does the actual preempting) would have happily settled.
        let preemptable =
            opts.preempt && sym_conflicts.iter().all(|c| c.priority < opts.priority);
        if !preemptable {
            conflicts.extend(sym_conflicts);
        }
    }
    if !conflicts.is_empty() {
        return Ok(conflicts);
    }
    for sym in scope {
        acquire_in_tx(tx, sym, agent, opts)?;
    }
    Ok(Vec::new())
}

pub(crate) fn conflicts_tx(
    tx: &Transaction,
    symbol_id: &str,
    agent: &str,
) -> Result<(Vec<Conflict>, Vec<Lease>)> {
    let ancestors = store::tx_ancestor_ids(tx, symbol_id)?;
    let descendants = store::tx_descendant_ids(tx, symbol_id)?;
    let mut conflicts: Vec<Conflict> = Vec::new();
    let mut blocking: Vec<Lease> = Vec::new();

    for (idx, id) in ancestors.iter().chain(descendants.iter()).enumerate() {
        let relation = if id == symbol_id {
            ConflictRelation::Same
        } else if idx < ancestors.len() {
            ConflictRelation::Ancestor
        } else {
            ConflictRelation::Descendant
        };
        for lease in active_leases_on(tx, id)? {
            if lease.agent == agent {
                continue; // our own nested lease never blocks us
            }
            conflicts.push(conflict_from(&lease, relation));
            blocking.push(lease);
        }
    }
    Ok((conflicts, blocking))
}

/// Take a lease inside an existing transaction, for callers that must do it
/// atomically alongside other work (the scheduler claiming a task).
pub(crate) fn acquire_in_tx(
    tx: &Transaction,
    symbol_id: &str,
    agent: &str,
    opts: &AcquireOptions,
) -> Result<AcquireOutcome> {
    acquire_tx(tx, symbol_id, agent, opts)
}

/// Release every active lease taken for a task, whoever holds it.
///
/// Called when the task reaches a terminal state: the reservation existed to
/// protect that work, and the work is over.
pub(crate) fn release_task_leases(tx: &Transaction, task_id: &str) -> Result<usize> {
    let mut stmt = tx.prepare(&format!(
        "{LEASE_SELECT} WHERE l.task = ?1 AND l.state = 'active'"
    ))?;
    let held: Vec<Lease> = stmt
        .query_map(params![task_id], row_to_lease)?
        .collect::<Result<Vec<_>, _>>()?;
    drop(stmt);

    for lease in &held {
        finish_lease(tx, lease, LeaseState::Released, "task finished")?;
    }
    Ok(held.len())
}

/// Move an active lease between agents inside an existing transaction.
pub(crate) fn transfer_lease_tx(
    tx: &Transaction,
    lease_id: &str,
    from: &str,
    to: &str,
) -> Result<Lease> {
    if from == to {
        return Err(anyhow!("{from} already holds that lease"));
    }
    let lease = load_lease(tx, lease_id)?.ok_or_else(|| anyhow!("no such lease: {lease_id}"))?;
    if lease.agent != from {
        return Err(anyhow!(
            "lease {lease_id} belongs to {}, not {from}",
            lease.agent
        ));
    }
    if lease.state != LeaseState::Active {
        return Err(anyhow!("lease {lease_id} is {}", lease.state.as_str()));
    }

    let now = ids::now_ms();
    tx.execute(
        "UPDATE leases SET agent = ?2, heartbeat_at = ?3, expires_at = ?4, acquired_at = ?3 \
         WHERE id = ?1",
        params![lease.id, to, now, now + lease.ttl_secs * 1000],
    )?;
    // The new owner is no longer waiting for what they now hold.
    tx.execute(
        "UPDATE lease_queue SET state = 'granted' WHERE agent = ?1 AND symbol_id = ?2 \
         AND state = 'waiting'",
        params![to, lease.symbol_id],
    )?;
    store::emit(
        tx,
        "lease.transferred",
        Some(to),
        Some(&lease.symbol_handle),
        lease.task.as_deref(),
        json!({ "lease": lease.id, "from": from, "to": to }),
    )?;
    load_lease(tx, lease_id)?.ok_or_else(|| anyhow!("lease vanished"))
}

/// The active lease `agent` holds on `symbol_id`, if any.
pub(crate) fn active_lease_for(
    tx: &Transaction,
    symbol_id: &str,
    agent: &str,
) -> Result<Option<Lease>> {
    Ok(active_leases_on(tx, symbol_id)?
        .into_iter()
        .find(|l| l.agent == agent))
}

fn active_lease_on(tx: &Transaction, symbol_id: &str) -> Result<Option<Lease>> {
    Ok(active_leases_on(tx, symbol_id)?.into_iter().next())
}

fn active_leases_on(tx: &Transaction, symbol_id: &str) -> Result<Vec<Lease>> {
    let mut stmt = tx.prepare(&format!(
        "{LEASE_SELECT} WHERE l.symbol_id = ?1 AND l.state = 'active'"
    ))?;
    let rows = stmt.query_map(params![symbol_id], row_to_lease)?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

fn leases_for_agent(tx: &Transaction, agent: &str) -> Result<Vec<Lease>> {
    let mut stmt = tx.prepare(&format!(
        "{LEASE_SELECT} WHERE l.agent = ?1 AND l.state = 'active'"
    ))?;
    let rows = stmt.query_map(params![agent], row_to_lease)?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

pub(crate) fn load_lease(tx: &Transaction, id: &str) -> Result<Option<Lease>> {
    Ok(tx
        .query_row(
            &format!("{LEASE_SELECT} WHERE l.id = ?1"),
            params![id],
            row_to_lease,
        )
        .optional()?)
}

pub(crate) fn conflict_from(lease: &Lease, relation: ConflictRelation) -> Conflict {
    Conflict {
        blocking_symbol: lease.symbol_handle.clone(),
        relation,
        holder: lease.agent.clone(),
        task: lease.task.clone(),
        priority: lease.priority,
        expires_at: lease.expires_at,
        seconds_until_free: lease.seconds_left(ids::now_ms()).max(0),
        lease_id: lease.id.clone(),
    }
}

fn relation_of(target: &str, other: &str, store: &Store) -> Result<ConflictRelation> {
    if target == other {
        return Ok(ConflictRelation::Same);
    }
    if store.ancestors(target)?.iter().any(|s| s.id == other) {
        return Ok(ConflictRelation::Ancestor);
    }
    Ok(ConflictRelation::Descendant)
}

fn row_to_lease(r: &Row) -> rusqlite::Result<Lease> {
    let state: String = r.get(10)?;
    Ok(Lease {
        id: r.get(0)?,
        symbol_id: r.get(1)?,
        symbol_handle: r.get(2)?,
        agent: r.get(3)?,
        task: r.get(4)?,
        priority: r.get(5)?,
        ttl_secs: r.get(6)?,
        acquired_at: r.get(7)?,
        heartbeat_at: r.get(8)?,
        expires_at: r.get(9)?,
        state: LeaseState::parse(&state).unwrap_or(LeaseState::Expired),
        note: r.get(11)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan;
    use std::path::PathBuf;

    /// A workspace with a class, its methods, and a free function.
    fn fixture() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("src/pay.ts"),
            "export class PaymentService {\n\
             \x20 processPayment(id: string) { return 1; }\n\
             \x20 refund(id: string) { return 2; }\n\
             }\n\
             export function audit() { return 3; }\n",
        )
        .unwrap();
        let mut store = Store::init(dir.path()).unwrap();
        scan::scan(&mut store, crate::ids::DEFAULT_REPO_ID, dir.path(), &[] as &[PathBuf], false).unwrap();
        (dir, store)
    }

    fn opts(ttl: i64) -> AcquireOptions {
        AcquireOptions {
            ttl_secs: ttl,
            ..Default::default()
        }
    }

    #[test]
    fn two_agents_cannot_hold_the_same_symbol() {
        let (_d, mut store) = fixture();
        let sym = store.resolve("PaymentService.processPayment").unwrap();

        let first = store.acquire(&sym.id, "agent-1", &opts(300)).unwrap();
        assert!(first.is_granted());

        let second = store.acquire(&sym.id, "agent-2", &opts(300)).unwrap();
        match second {
            AcquireOutcome::Denied { conflicts } => {
                assert_eq!(conflicts.len(), 1);
                assert_eq!(conflicts[0].holder, "agent-1");
                assert_eq!(conflicts[0].relation, ConflictRelation::Same);
                assert!(conflicts[0].seconds_until_free > 0);
            }
            other => panic!("expected denial, got {other:?}"),
        }
        assert_eq!(store.active_leases(None).unwrap().len(), 1);
    }

    #[test]
    fn re_acquiring_your_own_lease_extends_it() {
        let (_d, mut store) = fixture();
        let sym = store.resolve("audit").unwrap();
        let a = store.acquire(&sym.id, "agent-1", &opts(60)).unwrap();
        let b = store.acquire(&sym.id, "agent-1", &opts(600)).unwrap();
        assert!(matches!(b, AcquireOutcome::Extended { .. }));
        assert!(b.lease().unwrap().expires_at > a.lease().unwrap().expires_at);
        assert_eq!(store.active_leases(None).unwrap().len(), 1);
    }

    #[test]
    fn sibling_symbols_do_not_conflict() {
        let (_d, mut store) = fixture();
        let a = store.resolve("PaymentService.processPayment").unwrap();
        let b = store.resolve("PaymentService.refund").unwrap();
        assert!(store.acquire(&a.id, "agent-1", &opts(300)).unwrap().is_granted());
        assert!(store.acquire(&b.id, "agent-2", &opts(300)).unwrap().is_granted());
    }

    #[test]
    fn holding_a_class_blocks_its_methods_and_the_reverse() {
        let (_d, mut store) = fixture();
        let class = store.resolve("PaymentService").unwrap();
        let method = store.resolve("PaymentService.refund").unwrap();

        assert!(store.acquire(&class.id, "agent-1", &opts(300)).unwrap().is_granted());
        let denied = store.acquire(&method.id, "agent-2", &opts(300)).unwrap();
        match denied {
            AcquireOutcome::Denied { conflicts } => {
                assert_eq!(conflicts[0].relation, ConflictRelation::Ancestor);
            }
            other => panic!("expected denial, got {other:?}"),
        }

        store.release_all("agent-1").unwrap();
        assert!(store.acquire(&method.id, "agent-2", &opts(300)).unwrap().is_granted());
        let denied = store.acquire(&class.id, "agent-3", &opts(300)).unwrap();
        match denied {
            AcquireOutcome::Denied { conflicts } => {
                assert_eq!(conflicts[0].relation, ConflictRelation::Descendant);
            }
            other => panic!("expected denial, got {other:?}"),
        }
    }

    #[test]
    fn leasing_a_file_blocks_everything_in_it() {
        let (_d, mut store) = fixture();
        let file = store.resolve("src/pay.ts").unwrap();
        let method = store.resolve("PaymentService.refund").unwrap();
        assert!(store.acquire(&file.id, "agent-1", &opts(300)).unwrap().is_granted());
        assert!(!store.acquire(&method.id, "agent-2", &opts(300)).unwrap().is_granted());
    }

    #[test]
    fn expired_leases_free_themselves() {
        let (_d, mut store) = fixture();
        let sym = store.resolve("audit").unwrap();
        // TTL of 0 means "expired the moment it was taken" — the crash case,
        // without having to sleep in a test.
        assert!(store.acquire(&sym.id, "crashed", &opts(0)).unwrap().is_granted());
        std::thread::sleep(std::time::Duration::from_millis(5));

        let taken = store.acquire(&sym.id, "agent-2", &opts(300)).unwrap();
        assert!(taken.is_granted(), "expected the dead agent's lease to expire");

        let kinds: Vec<String> = store
            .recent_events(20)
            .unwrap()
            .into_iter()
            .map(|e| e.kind)
            .collect();
        assert!(kinds.contains(&"lease.expired".to_string()), "{kinds:?}");
    }

    #[test]
    fn heartbeat_pushes_expiry_out() {
        let (_d, mut store) = fixture();
        let sym = store.resolve("audit").unwrap();
        let lease = store
            .acquire(&sym.id, "agent-1", &opts(2))
            .unwrap()
            .lease()
            .unwrap()
            .clone();
        std::thread::sleep(std::time::Duration::from_millis(20));
        let renewed = store.heartbeat("agent-1", None).unwrap();
        assert_eq!(renewed.len(), 1);
        assert!(renewed[0].expires_at > lease.expires_at);
    }

    #[test]
    fn only_the_owner_can_release() {
        let (_d, mut store) = fixture();
        let sym = store.resolve("audit").unwrap();
        let lease = store
            .acquire(&sym.id, "agent-1", &opts(300))
            .unwrap()
            .lease()
            .unwrap()
            .clone();
        assert!(store.release(&lease.id, "agent-2").is_err());
        assert!(store.release(&lease.id, "agent-1").is_ok());
        assert!(store.active_leases(None).unwrap().is_empty());
    }

    #[test]
    fn higher_priority_can_preempt() {
        let (_d, mut store) = fixture();
        let sym = store.resolve("audit").unwrap();
        store
            .acquire(
                &sym.id,
                "background",
                &AcquireOptions {
                    priority: 1,
                    ..opts(300)
                },
            )
            .unwrap();

        let same_priority = store.acquire(
            &sym.id,
            "hotfix",
            &AcquireOptions {
                priority: 1,
                preempt: true,
                ..opts(300)
            },
        );
        assert!(!same_priority.unwrap().is_granted(), "equal priority must not preempt");

        let higher = store
            .acquire(
                &sym.id,
                "hotfix",
                &AcquireOptions {
                    priority: 9,
                    preempt: true,
                    ..opts(300)
                },
            )
            .unwrap();
        assert!(higher.is_granted());
        let active = store.active_leases(None).unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].agent, "hotfix");
    }

    #[test]
    fn queue_gives_the_first_waiter_the_lease() {
        let (_d, mut store) = fixture();
        let sym = store.resolve("audit").unwrap();
        store.acquire(&sym.id, "holder", &opts(300)).unwrap();

        // Two agents queue up, in order.
        assert!(!store.acquire(&sym.id, "waiter-1", &opts(300)).unwrap().is_granted());
        assert!(!store.acquire(&sym.id, "waiter-2", &opts(300)).unwrap().is_granted());
        let queue = store.queue_for(&sym.id).unwrap();
        assert_eq!(queue.len(), 2);
        assert_eq!(queue[0].agent, "waiter-1");

        store.release_all("holder").unwrap();
        // The second waiter must not jump the line.
        let jump = store.acquire(&sym.id, "waiter-2", &opts(300)).unwrap();
        match jump {
            AcquireOutcome::Denied { conflicts } => {
                assert_eq!(conflicts[0].relation, ConflictRelation::Queued);
                assert_eq!(conflicts[0].holder, "waiter-1");
            }
            other => panic!("expected queue denial, got {other:?}"),
        }
        assert!(store.acquire(&sym.id, "waiter-1", &opts(300)).unwrap().is_granted());
        assert!(store.queue_for(&sym.id).unwrap().iter().all(|q| q.agent != "waiter-1"));
    }

    #[test]
    fn conflicts_can_be_previewed_without_side_effects() {
        let (_d, mut store) = fixture();
        let sym = store.resolve("audit").unwrap();
        store.acquire(&sym.id, "holder", &opts(300)).unwrap();
        let before = store.waiting_count().unwrap();
        let conflicts = store.conflicts_for(&sym.id, "curious").unwrap();
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].holder, "holder");
        assert_eq!(store.waiting_count().unwrap(), before, "preview must not queue");
    }
}
