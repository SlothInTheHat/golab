//! Terminal output. Every command also speaks `--json` so agents can parse
//! exactly what humans see.

use std::io::IsTerminal;

use chrono::{Local, TimeZone};
use atlas_core::activity::{self, ActivityView};
use atlas_core::arch::{ArchGraph, ArchKind, ArchNode, NodeDetail};
use atlas_core::context::{AgentContext, TaskContext};
use atlas_core::guard::{GuardReport, GuardVerdict};
use atlas_core::model::*;
use atlas_core::session::SessionView;
use atlas_core::worker::{Worker, WorkerStatus, WorkerType};
use atlas_core::work::{AgentView, StatusSummary, TaskView};

pub struct Style {
    color: bool,
}

impl Style {
    pub fn detect() -> Style {
        let color = std::io::stdout().is_terminal()
            && std::env::var_os("NO_COLOR").is_none()
            && std::env::var("TERM").map(|t| t != "dumb").unwrap_or(true);
        Style { color }
    }

    pub fn plain() -> Style {
        Style { color: false }
    }

    fn wrap(&self, code: &str, s: &str) -> String {
        if self.color {
            format!("\x1b[{code}m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    }

    pub fn bold(&self, s: &str) -> String {
        self.wrap("1", s)
    }
    pub fn dim(&self, s: &str) -> String {
        self.wrap("2", s)
    }
    pub fn red(&self, s: &str) -> String {
        self.wrap("31", s)
    }
    pub fn green(&self, s: &str) -> String {
        self.wrap("32", s)
    }
    pub fn yellow(&self, s: &str) -> String {
        self.wrap("33", s)
    }
    pub fn blue(&self, s: &str) -> String {
        self.wrap("36", s)
    }
}

pub fn timestamp(ms: i64) -> String {
    match Local.timestamp_millis_opt(ms).single() {
        Some(dt) => dt.format("%H:%M:%S").to_string(),
        None => "--:--:--".to_string(),
    }
}

pub fn duration(secs: i64) -> String {
    if secs < 0 {
        return "expired".to_string();
    }
    if secs < 60 {
        return format!("{secs}s");
    }
    let m = secs / 60;
    if m < 60 {
        format!("{m}m{:02}s", secs % 60)
    } else {
        format!("{}h{:02}m", m / 60, m % 60)
    }
}

pub fn symbol_line(s: &Symbol, st: &Style) -> String {
    let tag = match (s.role, s.route()) {
        (_, Some(route)) => st.yellow(&format!(" [{route}]")),
        (Some(role), None) => st.dim(&format!(" [{}]", role.as_str())),
        (None, None) => String::new(),
    };
    format!(
        "{:<10} {}{}  {}",
        st.dim(s.kind.as_str()),
        st.bold(&s.handle()),
        tag,
        st.dim(&format!("{}:{}", s.path, s.start_line + 1))
    )
}

pub fn endpoint_line(s: &Symbol, st: &Style) -> String {
    let method = s.meta["method"].as_str().unwrap_or("ANY");
    let path = s.meta["path"].as_str().unwrap_or("");
    format!(
        "{:<7} {:<32} {} {}",
        st.green(method),
        st.bold(path),
        s.fqn,
        st.dim(&s.location())
    )
}

pub fn scan_line(stats: &atlas_core::scan::ScanStats, st: &Style) -> String {
    let mut extra = Vec::new();
    if stats.services > 0 {
        extra.push(format!("{} service(s)", stats.services));
    }
    if stats.tables > 0 {
        extra.push(format!("{} table(s)", stats.tables));
    }
    if stats.endpoints > 0 {
        extra.push(format!("{} endpoint(s)", stats.endpoints));
    }
    format!(
        "{} {} file(s), {} unchanged{} {}",
        st.green("indexed"),
        stats.files_indexed,
        stats.files_unchanged,
        if extra.is_empty() {
            String::new()
        } else {
            format!(" · {}", extra.join(", "))
        },
        st.dim(&format!("({}ms)", stats.elapsed_ms))
    )
}

pub fn lease_line(l: &Lease, now: i64, st: &Style) -> String {
    let left = l.seconds_left(now);
    let ttl = if left <= 10 {
        st.red(&duration(left))
    } else if left <= 60 {
        st.yellow(&duration(left))
    } else {
        st.green(&duration(left))
    };
    format!(
        "{}  {:<28} {:<14} {:<8} {}",
        st.dim(&l.id),
        st.bold(&l.symbol_handle),
        st.blue(&l.agent),
        ttl,
        l.task
            .as_deref()
            .map(|t| st.dim(&format!("task={t}")))
            .unwrap_or_default()
    )
}

pub fn conflict_lines(conflicts: &[Conflict], st: &Style) -> String {
    let mut out = String::new();
    for c in conflicts {
        let why = match c.relation {
            ConflictRelation::Same => "held".to_string(),
            ConflictRelation::Ancestor => format!("inside {}, held", c.blocking_symbol),
            ConflictRelation::Descendant => format!("contains {}, held", c.blocking_symbol),
            ConflictRelation::Queued => "queued behind".to_string(),
        };
        let when = if c.relation == ConflictRelation::Queued {
            "waiting".to_string()
        } else {
            format!("frees in ~{}", duration(c.seconds_until_free))
        };
        out.push_str(&format!(
            "  {} {} by {} {}\n",
            st.red("✗"),
            why,
            st.bold(&c.holder),
            st.dim(&format!(
                "({when}{})",
                c.task
                    .as_deref()
                    .map(|t| format!(", task={t}"))
                    .unwrap_or_default()
            ))
        ));
    }
    out
}

pub fn event_line(e: &Event, st: &Style) -> String {
    let icon = match e.kind.as_str() {
        "lease.acquired" => st.green("●"),
        "lease.released" => st.dim("○"),
        "lease.expired" | "lease.dropped" => st.yellow("◌"),
        "lease.denied" => st.red("✗"),
        "lease.preempted" => st.red("⇅"),
        "lease.queued" => st.yellow("…"),
        "lease.transferred" => st.blue("⇄"),
        "request.opened" => st.yellow("?"),
        "request.accepted" => st.blue("↻"),
        "request.fulfilled" => st.green("✔"),
        "request.declined" | "request.expired" => st.red("✗"),
        "agent.progress" => st.blue("▸"),
        "task.completed" => st.green("✔"),
        "task.blocked" | "task.failed" => st.red("■"),
        "agent.joined" => st.blue("+"),
        "agent.left" => st.dim("-"),
        "session.started" => st.blue("⇱"),
        "session.ended" => st.dim("⇲"),
        "session.expired" => st.yellow("◌"),
        "guard.denied" => st.red("⛔"),
        "activity.started" => st.blue("✎"),
        "activity.ended" => st.dim("✎"),
        _ => st.dim("·"),
    };
    let who = e.agent.as_deref().unwrap_or("-");
    let what = e.symbol_handle.as_deref().unwrap_or("");
    let extra = match e.kind.as_str() {
        "lease.denied" => e.detail["conflicts"][0]["holder"]
            .as_str()
            .map(|h| format!("blocked by {h}"))
            .unwrap_or_default(),
        "lease.acquired" => e.detail["ttl_secs"]
            .as_i64()
            .map(|t| format!("ttl={t}s"))
            .unwrap_or_default(),
        "task.created" | "task.started" | "task.completed" | "task.unblocked" => e.detail["title"]
            .as_str()
            .map(|s| s.to_string())
            .unwrap_or_else(|| e.task.clone().unwrap_or_default()),
        "scan.completed" => format!(
            "{} files, {} symbols",
            e.detail["files_indexed"], e.detail["symbols"]
        ),
        "lease.transferred" => e.detail["from"]
            .as_str()
            .map(|f| format!("from {f}"))
            .unwrap_or_default(),
        "request.opened" => e.detail["subject"]
            .as_str()
            .map(|s| s.to_string())
            .unwrap_or_default(),
        "request.accepted" | "request.fulfilled" | "request.declined" | "request.expired" => {
            e.detail["requester"]
                .as_str()
                .map(|r| format!("for {r}"))
                .unwrap_or_default()
        }
        "agent.progress" => {
            let pct = e.detail["percent"].as_i64();
            let note = e.detail["note"].as_str().unwrap_or("");
            match pct {
                Some(p) => format!("{p}% {note}"),
                None => note.to_string(),
            }
        }
        "session.started" | "session.ended" | "session.expired" => {
            let tool = e.detail["tool"].as_str().unwrap_or("");
            let via = e.detail["transport"].as_str().unwrap_or("");
            format!("{tool} via {via}")
        }
        "guard.denied" => e.detail["holder"]
            .as_str()
            .map(|h| format!("blocked by {h}"))
            .unwrap_or_default(),
        _ => String::new(),
    };
    format!(
        "{} {} {:<18} {:<14} {} {}",
        st.dim(&timestamp(e.ts)),
        icon,
        e.kind,
        st.blue(who),
        st.bold(what),
        st.dim(&extra)
    )
}

pub fn agent_line(a: &AgentView, st: &Style) -> String {
    let dot = if a.online {
        st.green("●")
    } else {
        st.dim("○")
    };
    format!(
        "{} {:<16} {:<10} {:<9} {}",
        dot,
        st.bold(&a.agent.name),
        st.dim(&a.agent.kind),
        a.agent.status,
        st.dim(&format!(
            "{} lease(s){}",
            a.leases,
            a.current_task
                .as_deref()
                .map(|t| format!(", task={t}"))
                .unwrap_or_default()
        ))
    )
}

pub fn swarm_line(a: &AgentView, st: &Style) -> String {
    let dot = if a.online {
        st.green("●")
    } else {
        st.dim("○")
    };
    let paused = if a.agent.paused {
        st.yellow(" [paused]")
    } else {
        String::new()
    };
    format!(
        "{} {:<16} {:<10}{} {}",
        dot,
        st.bold(&a.agent.name),
        st.dim(&a.agent.kind),
        paused,
        st.dim(&format!(
            "{} lease(s){}{}{}",
            a.leases,
            a.current_task
                .as_deref()
                .map(|t| format!(", task={t}"))
                .unwrap_or_default(),
            a.current_goal
                .as_deref()
                .map(|g| format!(", goal={g}"))
                .unwrap_or_default(),
            a.agent
                .capability
                .map(|c| format!(", capability={}", c.as_str()))
                .unwrap_or_default()
        ))
    )
}

pub fn goal_line(g: &Goal, progress: &GoalProgress, st: &Style) -> String {
    let marker = match g.state {
        GoalState::Done => st.green("✔"),
        GoalState::Abandoned => st.dim("○"),
        GoalState::Open if progress.total > 0 => st.blue("▶"),
        GoalState::Open => st.yellow("○"),
    };
    format!(
        "{} {:<5} {:<40} {}",
        marker,
        st.bold(&g.id),
        g.title,
        st.dim(&format!(
            "{}/{} tasks ({:.0}%)",
            progress.done, progress.total, progress.percent
        ))
    )
}

pub fn goal_progress_block(g: &Goal, progress: &GoalProgress, tasks: &[TaskView], st: &Style) -> String {
    let mut out = format!("{}\n", goal_line(g, progress, st));
    if let Some(d) = &g.description {
        out.push_str(&format!("  {}\n", st.dim(d)));
    }
    if !progress.contributing_agents.is_empty() {
        out.push_str(&format!(
            "  {} {}\n",
            st.dim("contributors"),
            progress.contributing_agents.join(", ")
        ));
    }
    if tasks.is_empty() {
        out.push_str(&format!("  {}\n", st.dim("no tasks yet — try `atlas goal decompose` or `atlas goal suggest`")));
    } else {
        for t in tasks {
            out.push_str(&format!("  {}\n", task_line(t, st)));
        }
    }
    out
}

pub fn suggestion_block(goal_id: &str, suggestions: &[TaskSuggestion], st: &Style) -> String {
    if suggestions.is_empty() {
        return format!("{}\n", st.dim("nothing in range to suggest"));
    }
    let mut out = format!(
        "{}\n",
        st.dim("advisory: where the impact lands, not what to do about it")
    );
    for s in suggestions {
        out.push_str(&format!(
            "  {} {}\n",
            st.bold(&s.title),
            st.dim(&format!("({} symbol(s))", s.symbols.len()))
        ));
        for sym in &s.symbols {
            out.push_str(&format!("    {}\n", st.dim(sym)));
        }
        out.push_str(&format!(
            "    {}\n",
            st.dim(&format!(
                "atlas goal decompose {goal_id} --task \"{}\" {}",
                s.title,
                s.symbols
                    .iter()
                    .map(|sym| format!("--symbol {sym}"))
                    .collect::<Vec<_>>()
                    .join(" ")
            ))
        ));
    }
    out
}

pub fn task_line(t: &TaskView, st: &Style) -> String {
    let marker = match t.task.state {
        TaskState::Done => st.green("✔"),
        TaskState::Running => st.blue("▶"),
        TaskState::Failed => st.red("✖"),
        TaskState::Blocked => st.red("■"),
        TaskState::Review => st.yellow("◐"),
        TaskState::Pending if t.ready => st.yellow("○"),
        TaskState::Pending => st.dim("◌"),
    };
    let suffix = if !t.blocked_by.is_empty() {
        st.dim(&format!(" blocked by {}", t.blocked_by.join(", ")))
    } else if let Some(a) = &t.task.assignee {
        st.dim(&format!(" → {a}"))
    } else {
        String::new()
    };
    let capability = match t.task.required_capability {
        Some(c) => st.dim(&format!(" [{}]", c.as_str())),
        None => String::new(),
    };
    format!(
        "{} {:<5} {:<40} {:<8} p{}{}{}",
        marker,
        st.bold(&t.task.id),
        t.task.title,
        t.task.state.as_str(),
        t.task.priority,
        suffix,
        capability
    )
}

pub fn request_line(r: &Request, st: &Style) -> String {
    let marker = match r.state {
        RequestState::Open => st.yellow("?"),
        RequestState::Accepted => st.blue("↻"),
        RequestState::Fulfilled => st.green("✔"),
        RequestState::Declined => st.red("✗"),
        RequestState::Cancelled => st.dim("○"),
        RequestState::Expired => st.dim("◌"),
    };
    let deadline = match r.seconds_until_deadline(atlas_core::now_ms()) {
        Some(s) if r.state.is_live() => format!(" · {} left", duration(s)),
        _ => String::new(),
    };
    format!(
        "{} {}  {:<16} {:<20} {}",
        marker,
        st.dim(&r.id),
        st.blue(&format!(
            "{}→{}",
            r.from,
            r.to.as_deref().unwrap_or("all")
        )),
        st.dim(&r.kind),
        format_args!("{}{}", st.bold(&r.subject), st.dim(&deadline))
    )
}

pub fn request_block(r: &Request, st: &Style) -> String {
    let mut out = format!("{}\n", request_line(r, st));
    out.push_str(&format!("  {} {}\n", st.dim("state"), r.state.as_str()));
    if let Some(h) = &r.resource_handle {
        out.push_str(&format!("  {} {}\n", st.dim("symbol"), st.bold(h)));
    }
    if let Some(t) = &r.resource_task {
        out.push_str(&format!("  {} {}\n", st.dim("awaiting"), st.bold(t)));
    }
    if r.body != serde_json::json!({}) {
        out.push_str(&format!("  {} {}\n", st.dim("body"), r.body));
    }
    if let Some(resp) = &r.response {
        out.push_str(&format!(
            "  {} {} {}\n",
            st.dim("answer"),
            resp,
            st.dim(&format!("by {}", r.resolver.as_deref().unwrap_or("-")))
        ));
    }
    out
}

pub fn progress_line(p: &ProgressUpdate, st: &Style) -> String {
    let bar = match p.percent {
        Some(pct) => {
            let filled = ((pct.clamp(0, 100) as usize) * 10) / 100;
            format!("[{}{}] {pct:>3}%", "█".repeat(filled), "·".repeat(10 - filled))
        }
        None => " ".repeat(16),
    };
    format!(
        "{:<14} {} {} {}",
        st.blue(&p.agent),
        st.green(&bar),
        p.note.as_deref().unwrap_or(""),
        st.dim(&format!(
            "{}{}",
            p.task
                .as_deref()
                .map(|t| format!("task={t} "))
                .unwrap_or_default(),
            p.eta_secs
                .map(|e| format!("eta {}", duration(e)))
                .unwrap_or_default()
        ))
    )
}

/// `2 symbols` / `src/pay.ts:charge` — short enough to sit on a task line.
pub fn scope_summary(scope: &[Symbol]) -> String {
    match scope.len() {
        0 => String::new(),
        1 => scope[0].handle(),
        n => format!("{n} symbols"),
    }
}

pub fn scheduled_line(t: &atlas_core::schedule::ScheduledTask, st: &Style) -> String {
    let scope = if t.scope.is_empty() {
        String::new()
    } else {
        st.dim(&format!("  ⟨{}⟩", scope_summary(&t.scope)))
    };
    let blocked = match &t.contended_by {
        Some(c) => st.red(&format!("  ✗ held by {}", c.holder)),
        None => String::new(),
    };
    format!("{}{}{}", task_line(&t.task, st), scope, blocked)
}

/// Answers the five questions the plan's vision calls out directly: what is
/// every agent doing, what's blocked and why, the critical path, who's idle,
/// and how much can run in parallel.
pub fn observe_block(
    agents: &[AgentView],
    tasks: &[TaskView],
    plan: &atlas_core::schedule::Plan,
    st: &Style,
) -> String {
    let mut out = String::new();

    let (idle, busy): (Vec<&AgentView>, Vec<&AgentView>) =
        agents.iter().partition(|a| a.current_task.is_none());
    out.push_str(&format!("{}\n", st.bold("who's doing what")));
    if agents.is_empty() {
        out.push_str(&format!("  {}\n", st.dim("nobody has joined this workspace")));
    }
    for a in &busy {
        out.push_str(&format!("  {}\n", swarm_line(a, st)));
    }
    if !idle.is_empty() {
        out.push_str(&format!(
            "  {} {}\n",
            st.dim("idle:"),
            idle.iter().map(|a| a.agent.name.as_str()).collect::<Vec<_>>().join(", ")
        ));
    }

    let blocked: Vec<&TaskView> = tasks
        .iter()
        .filter(|t| !t.ready && !t.blocked_by.is_empty())
        .collect();
    if !blocked.is_empty() || !plan.cycles.is_empty() {
        out.push_str(&format!("\n{}\n", st.bold("blocked")));
        for t in &blocked {
            out.push_str(&format!("  {}\n", task_line(t, st)));
        }
        for cycle in &plan.cycles {
            out.push_str(&format!(
                "  {} dependency cycle: {}\n",
                st.red("↻"),
                cycle.join(" → ")
            ));
        }
    }
    let contended: Vec<&atlas_core::schedule::ScheduledTask> = plan
        .waves
        .first()
        .map(|w| w.tasks.iter().filter(|t| t.contended_by.is_some()).collect())
        .unwrap_or_default();
    if !contended.is_empty() {
        for t in &contended {
            let holder = t.contended_by.as_ref().map(|c| c.holder.as_str()).unwrap_or("?");
            out.push_str(&format!(
                "  {} {} ready, but held by {}\n",
                st.yellow("✗"),
                st.bold(&t.task.task.id),
                st.bold(holder)
            ));
        }
    }

    if !plan.critical_path.is_empty() {
        out.push_str(&format!(
            "\n{} {}\n",
            st.bold("critical path"),
            st.dim("(longest not-yet-done chain; work already in progress is invisible to it)")
        ));
        out.push_str(&format!("  {}\n", plan.critical_path.join(" → ")));
    }

    out.push_str(&format!(
        "\n{} startable now · up to {} in parallel · {} in review\n",
        plan.startable_now,
        plan.max_parallel,
        plan.in_review.len()
    ));
    out
}

pub fn plan_block(plan: &atlas_core::schedule::Plan, st: &Style) -> String {
    let mut out = String::new();

    if !plan.running.is_empty() {
        out.push_str(&format!("{}\n", st.bold("in flight")));
        for t in &plan.running {
            out.push_str(&format!("  {}\n", scheduled_line(t, st)));
        }
        out.push('\n');
    }

    if plan.waves.is_empty() && plan.running.is_empty() {
        out.push_str(&format!("{}\n", st.dim("nothing scheduled")));
    }

    for wave in &plan.waves {
        let startable = wave.tasks.iter().filter(|t| t.startable()).count();
        let label = if wave.level == 0 {
            format!(
                "wave 1 · can start now ({startable} of {})",
                wave.tasks.len()
            )
        } else {
            format!("wave {} · after wave {}", wave.level + 1, wave.level)
        };
        out.push_str(&format!("{}\n", st.bold(&label)));
        for t in &wave.tasks {
            out.push_str(&format!("  {}\n", scheduled_line(t, st)));
        }
        out.push('\n');
    }

    if !plan.cycles.is_empty() {
        out.push_str(&format!(
            "{}\n",
            st.red("unschedulable — dependency cycle")
        ));
        for cycle in &plan.cycles {
            out.push_str(&format!("  {} {}\n", st.red("↻"), cycle.join(" → ")));
        }
        out.push('\n');
    }

    out.push_str(&st.dim(&format!(
        "{} startable now · up to {} in parallel · {} in flight\n",
        plan.startable_now,
        plan.max_parallel,
        plan.running.len()
    )));
    out
}

pub fn throughput_block(t: &atlas_core::work::Throughput, st: &Style) -> String {
    let mean = match t.mean_task_secs {
        Some(s) => duration(s.round() as i64),
        None => "—".to_string(),
    };
    // A coarse sparkline: eight block heights is plenty to see a trend.
    const BLOCKS: [char; 8] = [' ', '\u{2581}', '\u{2582}', '\u{2584}', '\u{2585}', '\u{2586}', '\u{2587}', '\u{2588}'];
    let peak = t.completed_series.iter().copied().max().unwrap_or(0).max(1);
    let spark: String = t
        .completed_series
        .iter()
        .map(|n| BLOCKS[((n * 7) / peak).clamp(0, 7) as usize])
        .collect();

    let mut out = format!("{}\n", st.bold(&format!("last {} minutes", t.window_minutes)));
    out.push_str(&format!(
        "  {:<18} {}\n",
        st.dim("tasks completed"),
        st.bold(&t.tasks_completed.to_string())
    ));
    out.push_str(&format!("  {:<18} {}\n", st.dim("mean duration"), mean));
    out.push_str(&format!(
        "  {:<18} {} started, {} reassigned\n",
        st.dim("task flow"),
        t.tasks_started,
        t.tasks_reassigned
    ));
    out.push_str(&format!(
        "  {:<18} {} acquired, {} denied, {} expired, {} handed over\n",
        st.dim("leases"),
        t.leases_acquired,
        if t.leases_denied > 0 {
            st.yellow(&t.leases_denied.to_string())
        } else {
            "0".to_string()
        },
        t.leases_expired,
        t.handovers
    ));
    out.push_str(&format!(
        "  {:<18} {}\n",
        st.dim("agents active"),
        t.agents_active
    ));
    out.push_str(&format!(
        "  {:<18} {} {}\n",
        st.dim("completions"),
        st.green(&spark),
        st.dim(&format!("(peak {peak})"))
    ));
    out
}

pub fn status_block(s: &StatusSummary, st: &Style) -> String {
    let now = atlas_core::now_ms();
    let mut out = String::new();
    out.push_str(&format!(
        "{}  {} files · {} symbols · {} edges\n",
        st.bold("index"),
        s.files,
        s.symbols,
        s.edges
    ));

    out.push_str(&format!("\n{}\n", st.bold("agents")));
    if s.agents.is_empty() {
        out.push_str(&format!("  {}\n", st.dim("none registered")));
    }
    for a in &s.agents {
        out.push_str(&format!("  {}\n", agent_line(a, st)));
    }

    out.push_str(&format!(
        "\n{} {}\n",
        st.bold("leases"),
        st.dim(&format!("({} waiting)", s.waiting))
    ));
    if s.leases.is_empty() {
        out.push_str(&format!("  {}\n", st.dim("none active")));
    }
    for l in &s.leases {
        out.push_str(&format!("  {}\n", lease_line(l, now, st)));
    }

    if !s.tasks.is_empty() {
        out.push_str(&format!("\n{}\n", st.bold("tasks")));
        for t in &s.tasks {
            out.push_str(&format!("  {}\n", task_line(t, st)));
        }
    }

    if !s.requests.is_empty() {
        out.push_str(&format!("\n{}\n", st.bold("negotiations")));
        for r in &s.requests {
            out.push_str(&format!("  {}\n", request_line(r, st)));
        }
    }

    if !s.progress.is_empty() {
        out.push_str(&format!("\n{}\n", st.bold("progress")));
        for p in &s.progress {
            out.push_str(&format!("  {}\n", progress_line(p, st)));
        }
    }

    if !s.recent_events.is_empty() {
        out.push_str(&format!("\n{}\n", st.bold("recent")));
        for e in &s.recent_events {
            out.push_str(&format!("  {}\n", event_line(e, st)));
        }
    }
    out
}

pub fn check_block(report: &CheckReport, st: &Style) -> String {
    let mut out = String::new();
    if report.changes.is_empty() {
        return format!("{} no symbol changes in the working tree\n", st.green("✔"));
    }
    if !report.violations.is_empty() {
        out.push_str(&format!(
            "{} {} unleased change(s) by {}\n",
            st.red("✗"),
            report.violations.len(),
            st.bold(&report.agent)
        ));
        for v in &report.violations {
            out.push_str(&format!(
                "  {} {:<10} {}\n",
                st.red(v.change.change.as_str()),
                v.change.kind.as_str(),
                st.bold(&v.change.handle)
            ));
            match &v.held_by {
                Some(c) => out.push_str(&format!(
                    "      {} held by {} for another {}\n",
                    st.red("→"),
                    st.bold(&c.holder),
                    duration(c.seconds_until_free)
                )),
                None => out.push_str(&format!(
                    "      {} run: atlas lease acquire {} --agent {}\n",
                    st.dim("→"),
                    v.change.handle,
                    report.agent
                )),
            }
        }
    }
    if !report.covered.is_empty() {
        out.push_str(&format!(
            "{} {} change(s) covered by your leases\n",
            st.green("✔"),
            report.covered.len()
        ));
        for c in &report.covered {
            out.push_str(&format!(
                "  {} {:<10} {} {}\n",
                st.green(c.change.change.as_str()),
                c.change.kind.as_str(),
                st.bold(&c.change.handle),
                st.dim(&format!("via {}", c.via))
            ));
        }
    }
    out
}

pub fn session_line(s: &SessionView, st: &Style) -> String {
    let dot = if s.live { st.green("●") } else { st.dim("○") };
    let when = match s.session.ended_at {
        Some(_) => "ended".to_string(),
        None if s.live => format!("up {}", duration(s.uptime_secs)),
        // Not ended and not live means nobody closed it and nobody is
        // heartbeating it — a crash the next sweep has yet to reach.
        None => "silent".to_string(),
    };
    format!(
        "{} {:<16} {:<14} {:<7} {:<10} {}",
        dot,
        st.bold(&s.session.agent),
        st.dim(&s.session.tool),
        s.session.transport,
        when,
        st.dim(&s.session.cwd)
    )
}

/// The architecture picture, for a terminal.
///
/// The dashboard draws this as a graph; here it is a list ordered so that a
/// dependency reads left to right, which is as close as a terminal gets.
pub fn arch_block(g: &ArchGraph, st: &Style) -> String {
    let mut out = String::new();
    if g.nodes.is_empty() {
        return format!("{}\n", st.dim("nothing indexed — run `atlas index`"));
    }

    let name_of = |id: &str| -> String {
        g.nodes
            .iter()
            .find(|n| n.id == id)
            .map(|n| n.name.clone())
            .unwrap_or_else(|| id.to_string())
    };

    for cycle in &g.cycles {
        out.push_str(&format!(
            "{} dependency cycle: {}\n",
            st.red("!"),
            cycle
                .iter()
                .map(|id| name_of(id))
                .collect::<Vec<_>>()
                .join(" → ")
        ));
    }

    let mut roots: Vec<&ArchNode> = g.nodes.iter().filter(|n| n.parent.is_none()).collect();
    roots.sort_by_key(|n| (n.kind == ArchKind::Table, n.name.clone()));

    for n in roots {
        out.push_str(&arch_line(n, g, &name_of, 0, st));
        let mut kids: Vec<&ArchNode> = g
            .nodes
            .iter()
            .filter(|k| k.parent.as_deref() == Some(n.id.as_str()))
            .collect();
        kids.sort_by_key(|k| k.name.clone());
        for k in kids {
            out.push_str(&arch_line(k, g, &name_of, 1, st));
            let mut grand: Vec<&ArchNode> = g
                .nodes
                .iter()
                .filter(|x| x.parent.as_deref() == Some(k.id.as_str()))
                .collect();
            grand.sort_by_key(|x| x.name.clone());
            for x in grand {
                out.push_str(&arch_line(x, g, &name_of, 2, st));
            }
        }
    }
    out
}

fn arch_line(
    n: &ArchNode,
    g: &ArchGraph,
    name_of: &dyn Fn(&str) -> String,
    indent: usize,
    st: &Style,
) -> String {
    let pad = "  ".repeat(indent);
    // A box somebody is inside reads differently at a glance from an idle one.
    let marker = if !n.workers.is_empty() {
        st.green("●")
    } else if n.kind == ArchKind::Table {
        st.dim("▤")
    } else {
        st.dim("○")
    };

    let mut meta: Vec<String> = Vec::new();
    if n.files > 0 {
        meta.push(format!("{} file(s)", n.files));
    }
    if n.endpoints > 0 {
        meta.push(format!("{} endpoint(s)", n.endpoints));
    }
    if n.review_pending > 0 {
        meta.push(format!("{} in review", n.review_pending));
    }

    let deps: Vec<String> = g
        .edges
        .iter()
        .filter(|e| e.from == n.id && e.kind == EdgeKind::Imports)
        .map(|e| name_of(&e.to))
        .collect();
    let tables: Vec<String> = g
        .edges
        .iter()
        .filter(|e| e.from == n.id && e.kind == EdgeKind::Queries)
        .map(|e| name_of(&e.to))
        .collect();

    let mut line = format!("{pad}{marker} {:<22} {}", st.bold(&n.name), st.dim(&meta.join(" · ")));
    if !deps.is_empty() {
        line.push_str(&format!("  {} {}", st.dim("→"), deps.join(", ")));
    }
    if !tables.is_empty() {
        line.push_str(&format!("  {} {}", st.dim("▤"), tables.join(", ")));
    }
    line.push('\n');

    for a in &n.activity {
        // Somebody who was *refused* an edit is not somebody editing, and
        // rendering the two the same would hide the contention.
        let mark = match a.activity.kind.as_str() {
            activity::kind::BLOCKED => st.red("⛔"),
            activity::kind::EDITED => st.green("✎"),
            _ => st.yellow("✎"),
        };
        line.push_str(&format!(
            "{pad}    {} {} {} {}\n",
            mark,
            st.bold(&a.activity.agent),
            st.dim(&a.activity.kind),
            st.dim(
                a.activity
                    .symbol_handle
                    .as_deref()
                    .unwrap_or(&a.activity.path)
            )
        ));
    }
    if !n.goals.is_empty() {
        line.push_str(&format!(
            "{pad}    {}\n",
            st.dim(&format!("goals: {}", n.goals.join(", ")))
        ));
    }
    line
}

pub fn arch_node_block(d: &NodeDetail, st: &Style) -> String {
    let mut out = format!(
        "{} {} {}\n",
        st.bold(&d.node.name),
        st.dim(d.node.kind.as_str()),
        st.dim(d.node.path.as_deref().unwrap_or(""))
    );
    let mut stat = vec![format!("{} file(s)", d.node.files), format!("{} symbol(s)", d.node.symbols)];
    if d.node.endpoints > 0 {
        stat.push(format!("{} endpoint(s)", d.node.endpoints));
    }
    if d.node.tests > 0 {
        stat.push(format!("{} test(s)", d.node.tests));
    }
    out.push_str(&format!("  {}\n", st.dim(&stat.join(" · "))));

    let section = |out: &mut String, title: &str, items: Vec<String>| {
        if items.is_empty() {
            return;
        }
        out.push_str(&format!("  {}\n", st.dim(title)));
        for i in items {
            out.push_str(&format!("    {i}\n"));
        }
    };

    section(
        &mut out,
        "who is in here",
        d.held
            .iter()
            .map(|l| format!("{} holds {}", st.bold(&l.agent), l.symbol_handle))
            .chain(d.node.activity.iter().map(|a| {
                format!(
                    "{} {} {}",
                    st.bold(&a.activity.agent),
                    a.activity.kind,
                    a.activity.symbol_handle.as_deref().unwrap_or(&a.activity.path)
                )
            }))
            .collect(),
    );
    let names = |refs: &[atlas_core::arch::ArchRef]| -> Vec<String> {
        refs.iter().map(|r| r.name.clone()).collect()
    };
    section(&mut out, "depends on", names(&d.depends_on));
    section(&mut out, "depended on by", names(&d.depended_on_by));
    section(&mut out, "tables", names(&d.tables));
    section(
        &mut out,
        "endpoints",
        d.routes.iter().map(|r| r.handle()).collect(),
    );
    section(
        &mut out,
        "work landing here",
        d.tasks
            .iter()
            .map(|t| format!("{} {} ({})", t.task.id, t.task.title, t.task.state.as_str()))
            .chain(d.goals.iter().map(|g| format!("{} {}", g.id, g.title)))
            .collect(),
    );
    out
}

pub fn worker_line(w: &Worker, st: &Style) -> String {
    // Status is the first thing read, so it gets the colour and the marker.
    let (mark, status) = match w.status {
        WorkerStatus::Working => (st.green("●"), st.green("working")),
        WorkerStatus::Blocked => (st.red("●"), st.red("blocked")),
        WorkerStatus::Reviewing => (st.blue("●"), st.blue("reviewing")),
        WorkerStatus::Idle => (st.dim("○"), st.dim("idle")),
        WorkerStatus::Offline => (st.dim("·"), st.dim("offline")),
    };
    let kind = match w.worker_type {
        WorkerType::Human => "human",
        WorkerType::Ai => "ai",
        WorkerType::Service => "service",
    };

    let mut line = format!(
        "{} {:<16} {:<8} {:<10}",
        mark,
        st.bold(&w.name),
        st.dim(kind),
        status
    );
    // The tool actually attached, not the kind it registered under.
    if let Some(t) = &w.tool {
        line.push_str(&format!(" {:<13}", st.dim(t)));
    } else {
        line.push_str(&" ".repeat(14));
    }

    // What they are on, most specific first: the symbol beats the file beats
    // the task title.
    let what = w
        .current_symbol
        .as_deref()
        .or(w.current_file.as_deref())
        .or(w.task_title.as_deref())
        .unwrap_or("");
    line.push_str(what);

    let mut meta: Vec<String> = Vec::new();
    if let Some(t) = &w.task {
        meta.push(t.clone());
    }
    if let Some(g) = &w.goal {
        meta.push(g.clone());
    }
    if let Some(p) = w.percent {
        meta.push(format!("{p}%"));
    }
    if let Some(e) = w.eta_secs {
        meta.push(format!("~{} left", duration(e)));
    }
    if let Some(b) = &w.blocked_by {
        meta.push(format!("waiting on {b}"));
    }
    if w.paused {
        meta.push("paused".to_string());
    }
    if !meta.is_empty() {
        line.push_str(&st.dim(&format!("  ({})", meta.join(" · "))));
    }
    line
}

pub fn activity_line(a: &ActivityView, st: &Style) -> String {
    let verb = match a.activity.kind.as_str() {
        activity::kind::BLOCKED => st.red("blocked"),
        activity::kind::EDITING => st.yellow("editing"),
        activity::kind::EDITED => st.green("edited"),
        other => st.dim(other),
    };
    let dot = if a.live { st.green("●") } else { st.dim("○") };
    let what = a
        .activity
        .symbol_handle
        .as_deref()
        .unwrap_or(&a.activity.path);
    let mut line = format!(
        "{} {:<16} {:<9} {}",
        dot,
        st.bold(&a.activity.agent),
        verb,
        what
    );
    // For a live window, how long they have been in there is the useful
    // number; for a stale one it is how long ago they stopped.
    line.push_str(&st.dim(&if a.live {
        let held = (atlas_core::now_ms() - a.activity.started_at) / 1000;
        format!("  for {}", duration(held))
    } else {
        format!("  {} ago", duration(a.age_secs))
    }));
    if let Some(t) = &a.activity.task {
        line.push_str(&st.dim(&format!("  task={t}")));
    }
    line
}

pub fn guard_block(r: &GuardReport, st: &Style) -> String {
    let mut out = String::new();
    let head = match r.verdict {
        GuardVerdict::Allowed => st.green("✔"),
        GuardVerdict::Warn => st.yellow("!"),
        GuardVerdict::Denied => st.red("⛔"),
    };
    out.push_str(&format!("{head} {}\n", r.summary));
    if !r.conflicts.is_empty() {
        out.push_str(&conflict_lines(&r.conflicts, st));
    }
    for s in &r.suggestions {
        out.push_str(&format!(
            "  {} {}\n",
            st.dim("→"),
            st.dim(&s.command)
        ));
    }
    out
}

pub fn task_context_block(c: &TaskContext, st: &Style) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "{} {}  {}\n",
        st.bold(&c.task.task.id),
        st.bold(&c.task.task.title),
        st.dim(c.task.task.state.as_str())
    ));
    if let (Some(g), Some(p)) = (&c.goal, &c.goal_progress) {
        out.push_str(&format!(
            "{}\n",
            st.dim(&format!(
                "  serves {} “{}” · {}/{} done ({:.0}%)",
                g.id, g.title, p.done, p.total, p.percent
            ))
        ));
    }

    out.push_str(&format!("\n{}\n", st.bold("scope")));
    for s in &c.scope {
        let holder = match &s.lease {
            Some(l) => st.dim(&format!("held by {}", l.agent)),
            None => st.yellow("unleased"),
        };
        out.push_str(&format!(
            "  {} {} {}\n",
            st.bold(&s.symbol.handle()),
            s.route.as_deref().map(|r| st.yellow(r)).unwrap_or_default(),
            holder
        ));
    }

    if !c.impact.is_empty() {
        out.push_str(&format!(
            "\n{} {}\n",
            st.bold("what this touches"),
            st.dim(&format!("({})", c.impact.len()))
        ));
        for n in c.impact.iter().take(10) {
            let owner = n
                .lease
                .as_ref()
                .map(|l| st.red(&format!(" held by {}", l.agent)))
                .unwrap_or_default();
            out.push_str(&format!(
                "  {} {}{}\n",
                st.dim(&format!("{}·", n.distance)),
                n.symbol.handle(),
                owner
            ));
        }
    }
    if !c.tests.is_empty() {
        out.push_str(&format!("\n{}\n", st.bold("tests that cover it")));
        for t in &c.tests {
            out.push_str(&format!("  {}\n", t.handle()));
        }
    }
    if !c.neighbors_at_work.is_empty() {
        out.push_str(&format!("\n{}\n", st.bold("working next door")));
        for a in &c.neighbors_at_work {
            out.push_str(&format!("  {}\n", swarm_line(a, st)));
        }
    }
    if !c.memory.is_empty() {
        out.push_str(&format!("\n{}\n", st.bold("what the team already decided")));
        for m in &c.memory {
            out.push_str(&format!(
                "  {} {}\n",
                st.bold(&m.key),
                st.dim(&first_line(&m.value))
            ));
        }
    }
    if !c.notices.is_empty() {
        out.push_str(&format!("\n{}\n", st.bold("unread notices")));
        for r in &c.notices {
            out.push_str(&format!(
                "  {} {} {}\n",
                st.yellow("?"),
                st.dim(&r.id),
                r.subject
            ));
        }
    }
    if !c.blocked_by.is_empty() {
        let ids: Vec<&str> = c.blocked_by.iter().map(|t| t.task.id.as_str()).collect();
        out.push_str(&format!(
            "\n{} {}\n",
            st.red("blocked by"),
            ids.join(", ")
        ));
    }
    out
}

pub fn agent_context_block(c: &AgentContext, st: &Style) -> String {
    let mut out = String::new();
    out.push_str(&format!("{}\n", swarm_line(&c.agent, st)));
    for s in c.sessions.iter().filter(|s| s.live) {
        out.push_str(&format!(
            "{}\n",
            st.dim(&format!(
                "  connected: {} via {}",
                s.session.tool, s.session.transport
            ))
        ));
    }

    if let Some(task) = &c.task {
        out.push('\n');
        out.push_str(&task_context_block(task, st));
        return out;
    }

    if !c.open_goals.is_empty() {
        out.push_str(&format!("\n{}\n", st.bold("open goals")));
        for g in &c.open_goals {
            out.push_str(&format!(
                "  {} {}  {}\n",
                st.bold(&g.goal.id),
                g.goal.title,
                st.dim(&format!(
                    "{}/{} ({:.0}%)",
                    g.progress.done, g.progress.total, g.progress.percent
                ))
            ));
        }
    }
    out.push_str(&format!(
        "\n{} {}\n",
        st.bold("you could start"),
        st.dim(&format!("({})", c.startable.len()))
    ));
    for t in c.startable.iter().take(10) {
        out.push_str(&format!(
            "  {} {}\n",
            st.bold(&t.task.task.id),
            t.task.task.title
        ));
    }
    if c.startable.is_empty() {
        out.push_str(&format!("  {}\n", st.dim("nothing ready right now")));
    }
    if !c.critical_path.is_empty() {
        let ids: Vec<&str> = c.critical_path.iter().map(|t| t.task.id.as_str()).collect();
        out.push_str(&format!(
            "\n{} {}\n",
            st.bold("critical path"),
            ids.join(" → ")
        ));
    }
    if !c.notices.is_empty() {
        out.push_str(&format!("\n{}\n", st.bold("unread notices")));
        for r in &c.notices {
            out.push_str(&format!(
                "  {} {} {}\n",
                st.yellow("?"),
                st.dim(&r.id),
                r.subject
            ));
        }
    }
    out
}

fn first_line(s: &str) -> String {
    let line = s.lines().next().unwrap_or("");
    if line.chars().count() > 64 {
        format!("{}…", line.chars().take(61).collect::<String>())
    } else {
        line.to_string()
    }
}
