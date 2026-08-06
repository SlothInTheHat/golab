//! `golab` — command line for the collaborative agent runtime.
//!
//! Designed to be driven by humans *and* by agents: every command accepts
//! `--json`, and the exit code carries the answer (0 = granted/clean,
//! 1 = denied/violations), so a shell-driven agent can branch on it.

mod hooks;
mod render;
mod settings;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{anyhow, bail, Context, Result};
use clap::{Parser, Subcommand};
use golab_core::guard::GuardVerdict;
use golab_core::lease::{AcquireOptions, DEFAULT_TTL_SECS};
use golab_core::model::*;
use golab_core::workspace::{check_workspace, diff_workspace, guard_workspace, scan_workspace};
use golab_core::{find_workspace, Store};
use render::Style;
use serde_json::json;

#[derive(Parser)]
#[command(
    name = "golab",
    version,
    about = "Git tracks code. golab tracks work: symbol leases, task graph and shared state for humans and AI agents.",
    disable_help_subcommand = true
)]
struct Cli {
    /// Workspace root (defaults to the nearest .golab/ ancestor).
    #[arg(long, global = true)]
    root: Option<PathBuf>,

    /// Machine-readable output.
    #[arg(long, global = true)]
    json: bool,

    /// Acting agent. Falls back to $GOLAB_AGENT, then .golab/agent.
    #[arg(long, global = true)]
    agent: Option<String>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create the runtime database in this directory.
    Init { path: Option<PathBuf> },
    /// Register and list the repositories in this workspace.
    #[command(subcommand)]
    Repo(RepoCmd),
    /// Index the repository into the symbol graph.
    Scan {
        paths: Vec<PathBuf>,
        /// Re-parse even files whose contents are unchanged.
        #[arg(long)]
        force: bool,
    },
    /// List indexed symbols.
    Symbols {
        /// Substring of name, qualified name or path.
        pattern: Option<String>,
        #[arg(long)]
        path: Option<String>,
        #[arg(long)]
        kind: Option<String>,
        /// Filter by role: api, test, schema, service.
        #[arg(long)]
        role: Option<String>,
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
    /// Show one symbol: location, leases, neighbours.
    Show { symbol: String },
    /// Keep the index current as files change.
    Index {
        /// Watch the tree and re-index on save instead of scanning once.
        #[arg(long)]
        watch: bool,
    },
    /// Every HTTP endpoint in the repository.
    Api {
        /// Substring of the route path.
        pattern: Option<String>,
    },
    /// Which tests cover a symbol (or every test, with no argument).
    Tests { symbol: Option<String> },
    /// Database tables, and who reads or writes them.
    Tables { name: Option<String> },
    /// Services discovered from manifests, and what they depend on.
    Services,
    /// Who owns a path: CODEOWNERS, plus whoever holds a lease right now.
    Owners { path: Option<String> },
    /// What else a change to this symbol could disturb, and who owns it.
    Graph {
        symbol: String,
        #[arg(long, default_value_t = 2)]
        depth: usize,
    },
    /// Register, list and retire agents.
    #[command(subcommand)]
    Agent(AgentCmd),
    /// Take, renew and hand back symbol leases.
    #[command(subcommand)]
    Lease(LeaseCmd),
    /// Refuse edits to symbols this agent has not leased.
    Check {
        paths: Vec<PathBuf>,
        /// Report changes without failing.
        #[arg(long)]
        warn_only: bool,
    },
    /// May this agent edit this path right now? The question `check` answers
    /// after the fact — ask it before the edit and there is still time to
    /// negotiate. Exits 1 only when somebody else holds it.
    Guard {
        path: PathBuf,
        /// Narrow to one symbol: id, `path:Fqn`, fqn or bare name.
        #[arg(long)]
        symbol: Option<String>,
        /// Treat an unleased-but-uncontended edit as a denial too.
        #[arg(long)]
        strict: bool,
    },
    /// Everything worth knowing before starting work, in one packet: the
    /// scope, its blast radius, the tests that cover it, who is next door,
    /// and the decisions the team already made.
    Context {
        /// A task id. Defaults to the acting agent's current task.
        #[arg(long)]
        task: Option<String>,
        /// Orient an agent rather than a task.
        #[arg(long)]
        agent: Option<String>,
        #[arg(long, default_value_t = 2)]
        depth: usize,
    },
    /// Live connections from coding tools.
    #[command(subcommand)]
    Session(SessionCmd),
    /// The repository as a human pictures it: services, what they depend on,
    /// the tables they touch — and who is working inside each one right now.
    Arch {
        /// 1 services, 2 + directories, 3 + files.
        #[arg(long, default_value_t = 1)]
        depth: usize,
        /// Restrict to one registered repository.
        #[arg(long)]
        repo: Option<String>,
        /// Everything behind one box, instead of the whole picture.
        #[arg(long)]
        node: Option<String>,
    },
    /// Who is editing what, right now — before any of it is committed.
    ///
    /// Reported by coding tools as they work, through MCP or editor hooks. A
    /// tool wired up with neither contributes presence but no activity.
    Activity {
        /// Include windows that have gone quiet but not yet been swept.
        #[arg(long)]
        all: bool,
        /// Only this agent's.
        #[arg(long)]
        agent: Option<String>,
    },
    /// Symbol-level diff of the working tree against the index.
    Diff { paths: Vec<PathBuf> },
    /// Follow the event bus.
    Watch {
        /// Print history from this event id onward.
        #[arg(long, default_value_t = 0)]
        since: i64,
        /// Print and exit instead of following.
        #[arg(long)]
        once: bool,
        #[arg(long, default_value_t = 20)]
        tail: usize,
    },
    /// Everything at a glance.
    Status {
        #[arg(long, default_value_t = 10)]
        events: usize,
    },
    /// The task graph.
    #[command(subcommand)]
    Task(TaskCmd),
    /// What can run now, what runs next, and what is blocked.
    Schedule {
        /// Work out dependencies from the code graph before planning.
        #[arg(long)]
        infer: bool,
    },
    /// What the swarm got done, from the event log.
    Throughput {
        #[arg(long, default_value_t = 30)]
        minutes: i64,
    },
    /// What a human actually asked for — the thing tasks exist to serve.
    #[command(subcommand)]
    Goal(GoalCmd),
    /// Who's here: humans and agents sharing this workspace.
    #[command(subcommand)]
    Swarm(SwarmCmd),
    /// Hand a task (or the next task under a goal) to an agent.
    Assign {
        /// A task id (T1) or a goal id (G1) — for a goal, its next
        /// startable task is chosen.
        id: String,
        #[arg(long)]
        to: String,
        /// Take contended scope from a strictly lower-priority holder.
        #[arg(long)]
        preempt: bool,
        #[arg(long, default_value_t = 0)]
        priority: i64,
    },
    /// What everyone is doing, what's blocked, and what can run in parallel.
    Observe {
        /// Restrict to one goal's tasks.
        #[arg(long)]
        goal: Option<String>,
    },
    /// Submit work for approval, approve it, or send it back.
    #[command(subcommand)]
    Review(ReviewCmd),
    /// Keep working: renew what you have, or claim the next thing to do.
    Continue {
        #[arg(long, default_value_t = DEFAULT_TTL_SECS)]
        ttl: i64,
        /// Only claim work under this goal.
        #[arg(long)]
        goal: Option<String>,
    },
    /// Shared project memory.
    #[command(subcommand)]
    Memory(MemoryCmd),
    /// Structured agent-to-agent messages.
    #[command(subcommand)]
    Msg(MsgCmd),
    /// Negotiate with other agents: ask, answer, hand over.
    #[command(subcommand)]
    Request(RequestCmd),
    /// Report where you are, so nobody has to ask.
    Progress {
        #[arg(long)]
        percent: Option<i64>,
        #[arg(long)]
        note: Option<String>,
        /// Seconds until you expect to be done.
        #[arg(long)]
        eta: Option<i64>,
        #[arg(long)]
        task: Option<String>,
        #[arg(long)]
        symbol: Option<String>,
    },
    /// Install the git pre-commit hook that runs `golab check`.
    #[command(subcommand)]
    Hook(HookCmd),
    /// Serve the HTTP API, event stream and dashboard.
    Serve {
        #[arg(long, default_value_t = 7373)]
        port: u16,
        #[arg(long, default_value = "127.0.0.1")]
        host: String,
        /// Do not keep the index current while serving.
        #[arg(long)]
        no_watch: bool,
    },
    /// Speak MCP on stdio so a coding tool drives golab directly.
    ///
    /// Not run by hand: a client launches it (`golab hook install --mcp`
    /// writes the config). It registers, heartbeats, renews leases and leaves
    /// on its own, so nobody has to run lease commands during a session.
    Mcp {
        /// Pin the agent name. Otherwise `$GOLAB_AGENT`, else `<tool>-<user>`.
        #[arg(long = "as")]
        as_agent: Option<String>,
        /// Tool slug, when the client's handshake does not name itself.
        #[arg(long)]
        tool: Option<String>,
        #[arg(long, default_value_t = golab_mcp::DEFAULT_HEARTBEAT_SECS)]
        heartbeat_secs: u64,
        /// Keep leases held on disconnect — for a session that will resume.
        #[arg(long)]
        keep_leases: bool,
    },
}

#[derive(Subcommand)]
enum RepoCmd {
    /// Register a repository under this workspace.
    Add {
        /// Relative to the directory containing .golab/ — not absolute, so
        /// the workspace stays portable across machines and checkouts.
        path: String,
        #[arg(long)]
        name: Option<String>,
    },
    List,
}

#[derive(Subcommand)]
enum AgentCmd {
    /// Join the workspace (also sets the default agent for this checkout).
    Register {
        name: String,
        #[arg(long, default_value = "agent")]
        kind: String,
        /// Role this agent specializes in: planner, backend, frontend,
        /// database, testing, security, documentation, reviewer.
        #[arg(long)]
        capability: Option<String>,
    },
    List,
    /// Stop handing this agent new work; it keeps what it has.
    Pause { name: Option<String> },
    /// Let the scheduler hand it work again.
    Resume { name: Option<String> },
    /// Release every lease and leave.
    Leave { name: Option<String> },
    /// Say "still alive": refreshes presence and every held lease.
    Heartbeat,
}

#[derive(Subcommand)]
enum SessionCmd {
    /// Which coding tools are attached, and which rows are just history.
    List {
        /// Hide sessions that have ended or gone quiet.
        #[arg(long)]
        live: bool,
    },
    /// Close a session by hand — for a tool that died without saying so and
    /// whose leases you do not want to wait out.
    End {
        id: String,
        /// Leave its leases held.
        #[arg(long)]
        keep_leases: bool,
    },
}

#[derive(Subcommand)]
enum LeaseCmd {
    /// Claim a symbol.
    Acquire {
        symbol: String,
        #[arg(long, default_value_t = DEFAULT_TTL_SECS)]
        ttl: i64,
        #[arg(long, default_value_t = 0)]
        priority: i64,
        #[arg(long)]
        task: Option<String>,
        #[arg(long)]
        note: Option<String>,
        /// Keep retrying for this many seconds before giving up.
        #[arg(long)]
        wait: Option<u64>,
        /// Take it from a strictly lower-priority holder.
        #[arg(long)]
        preempt: bool,
        /// Do not record a place in the wait queue when denied.
        #[arg(long)]
        no_queue: bool,
    },
    /// Hand a lease back.
    Release {
        /// Lease id or symbol reference.
        target: Option<String>,
        #[arg(long)]
        all: bool,
    },
    /// Extend the TTL of everything this agent holds.
    Renew {
        #[arg(long)]
        lease: Option<String>,
    },
    /// Show active leases.
    List {
        /// Only this agent's leases.
        #[arg(long)]
        mine: bool,
    },
    /// Hand a lease to another agent without ever letting it go free.
    Transfer {
        /// Lease id or symbol reference.
        target: String,
        #[arg(long)]
        to: String,
    },
    /// Who is waiting for a symbol.
    Queue { symbol: String },
    /// Preview conflicts without taking or queueing anything.
    Check { symbol: String },
}

#[derive(Subcommand)]
enum TaskCmd {
    Add {
        title: String,
        #[arg(long, default_value_t = 0)]
        priority: i64,
        /// Task ids this one depends on.
        #[arg(long = "dep")]
        deps: Vec<String>,
        /// Symbols this task will touch. Repeatable.
        #[arg(long = "symbol")]
        symbols: Vec<String>,
        /// Only offer this task to an agent with this capability.
        #[arg(long)]
        capability: Option<String>,
    },
    List,
    /// Declare which symbols a task will touch.
    Scope {
        id: String,
        #[arg(long = "symbol", required = true)]
        symbols: Vec<String>,
    },
    /// Move a task to another agent, taking its leases with it.
    Assign {
        id: String,
        #[arg(long)]
        to: String,
    },
    /// Change a task's priority. Higher runs sooner.
    Priority { id: String, priority: i64 },
    /// Claim the best task you can safely start, and lease its scope.
    Next {
        #[arg(long, default_value_t = DEFAULT_TTL_SECS)]
        ttl: i64,
    },
    Done {
        id: String,
        /// Immediately claim the next task you can start.
        #[arg(long)]
        next: bool,
        /// Close a task assigned to somebody else.
        #[arg(long)]
        force: bool,
    },
    Block {
        id: String,
        reason: Option<String>,
    },
    Fail {
        id: String,
        reason: Option<String>,
        /// Fail a task assigned to somebody else.
        #[arg(long)]
        force: bool,
    },
}

#[derive(Subcommand)]
enum GoalCmd {
    Add {
        title: String,
        #[arg(long, default_value_t = 0)]
        priority: i64,
        #[arg(long)]
        description: Option<String>,
    },
    List,
    /// A goal's progress and the tasks under it.
    Show { id: String },
    /// Declare a subtask under a goal — the explicit half of decomposition.
    Decompose {
        id: String,
        #[arg(long)]
        task: String,
        #[arg(long, default_value_t = 0)]
        priority: i64,
        /// Task ids this subtask depends on.
        #[arg(long = "dep")]
        deps: Vec<String>,
        /// Symbols this subtask will touch. Repeatable.
        #[arg(long = "symbol")]
        symbols: Vec<String>,
        /// Only offer this subtask to an agent with this capability.
        #[arg(long)]
        capability: Option<String>,
    },
    /// Propose a task breakdown from the knowledge graph's impact radius.
    /// Advisory: names where a change lands, not what to do about it — there
    /// is no model here to invent that. The other half of decomposition.
    Suggest {
        id: String,
        /// Cluster the impact radius of this symbol.
        #[arg(long)]
        near: String,
        #[arg(long, default_value_t = 2)]
        depth: usize,
        /// Create the suggested tasks immediately instead of just previewing.
        #[arg(long)]
        apply: bool,
    },
    /// Propose a task breakdown straight from the goal's title and
    /// description, with no `--near` symbol required. Advisory, like
    /// `suggest` — a keyword match over the graph, not a generated plan.
    Plan {
        id: String,
        #[arg(long, default_value_t = 2)]
        depth: usize,
        /// Create the suggested tasks immediately instead of just previewing.
        #[arg(long)]
        apply: bool,
    },
    Done { id: String },
    Abandon { id: String },
}

#[derive(Subcommand)]
enum SwarmCmd {
    /// Join the workspace (also sets the default agent for this checkout).
    Join {
        name: String,
        #[arg(long, default_value = "agent")]
        kind: String,
        /// Role this agent specializes in: planner, backend, frontend,
        /// database, testing, security, documentation, reviewer.
        #[arg(long)]
        capability: Option<String>,
    },
    /// Everyone in the workspace: online state, current task, and its goal.
    List,
    Pause { name: Option<String> },
    Resume { name: Option<String> },
    Leave { name: Option<String> },
}

#[derive(Subcommand)]
enum ReviewCmd {
    /// Declare your work ready. Leases are kept — nothing is merged yet.
    Submit { id: String },
    /// Approve it: releases the leases and unblocks anyone waiting on it.
    Approve {
        id: String,
        /// Approve your own submission.
        #[arg(long)]
        force: bool,
    },
    /// Send it back, with a reason the assignee is notified of.
    Reject { id: String, reason: Option<String> },
    /// Tasks currently awaiting review.
    List,
}

#[derive(Subcommand)]
enum MemoryCmd {
    Set {
        key: String,
        value: String,
        #[arg(long = "tag")]
        tags: Vec<String>,
    },
    Get {
        key: String,
    },
    List {
        #[arg(long)]
        tag: Option<String>,
    },
    Rm {
        key: String,
    },
}

#[derive(Subcommand)]
enum MsgCmd {
    Send {
        #[arg(long)]
        to: String,
        #[arg(long)]
        subject: String,
        /// JSON object, or plain text (wrapped as {"text": ...}).
        #[arg(long, default_value = "{}")]
        body: String,
        #[arg(long)]
        reply_to: Option<i64>,
    },
    Inbox {
        /// Include messages already read.
        #[arg(long)]
        all: bool,
    },
    Read {
        id: i64,
    },
}

#[derive(Subcommand)]
enum RequestCmd {
    /// Ask the current holder of a symbol to hand it over.
    Lease {
        symbol: String,
        #[arg(long)]
        reason: Option<String>,
        /// Give up after this many seconds.
        #[arg(long, default_value_t = 300)]
        deadline: i64,
        #[arg(long, default_value_t = 0)]
        priority: i64,
        #[arg(long)]
        task: Option<String>,
        /// Block until the request is answered, then exit 0 if you got it.
        #[arg(long)]
        wait: Option<u64>,
    },
    /// Ask another agent for an interface you need before you can continue.
    Interface {
        /// What you need, e.g. PaymentProvider.
        resource: String,
        #[arg(long)]
        to: Option<String>,
        /// Method the interface must expose. Repeatable.
        #[arg(long = "method")]
        methods: Vec<String>,
        #[arg(long, default_value_t = 300)]
        deadline: i64,
        #[arg(long, default_value_t = 0)]
        priority: i64,
        #[arg(long)]
        task: Option<String>,
    },
    /// Declare that you are blocked until a task finishes.
    Depend {
        /// Task id you are waiting on.
        #[arg(long = "on-task")]
        on_task: String,
        #[arg(long)]
        to: Option<String>,
        #[arg(long)]
        note: Option<String>,
        #[arg(long, default_value_t = 900)]
        deadline: i64,
    },
    /// Open an arbitrary structured request.
    Open {
        #[arg(long, default_value = "question")]
        kind: String,
        #[arg(long)]
        to: Option<String>,
        #[arg(long)]
        subject: String,
        /// JSON object, or plain text (wrapped as {"text": ...}).
        #[arg(long, default_value = "{}")]
        body: String,
        #[arg(long)]
        symbol: Option<String>,
        #[arg(long = "about-task")]
        about_task: Option<String>,
        #[arg(long)]
        task: Option<String>,
        #[arg(long, default_value_t = 0)]
        priority: i64,
        #[arg(long)]
        deadline: Option<i64>,
    },
    /// Requests addressed to you (plus broadcasts).
    Inbox {
        /// Include requests already answered.
        #[arg(long)]
        all: bool,
    },
    /// Requests you opened.
    Outbox {
        #[arg(long)]
        all: bool,
    },
    /// Every request in the workspace.
    List {
        #[arg(long)]
        all: bool,
    },
    Show {
        id: String,
    },
    /// Say yes. For a lease request this performs the handover.
    Accept {
        id: String,
        #[arg(long, default_value = "{}")]
        body: String,
    },
    /// Say no, with a reason the other agent can act on.
    Decline {
        id: String,
        #[arg(long)]
        reason: Option<String>,
    },
    /// Deliver what was asked for.
    Fulfill {
        id: String,
        #[arg(long, default_value = "{}")]
        body: String,
    },
    /// Withdraw your own request.
    Cancel { id: String },
    /// Block until a request is answered. Exit 0 fulfilled, 1 otherwise.
    Wait {
        id: String,
        #[arg(long, default_value_t = 120)]
        timeout: u64,
    },
}

#[derive(Subcommand)]
enum HookCmd {
    /// Install integrations. With no flag this installs the git pre-commit
    /// hook, which is all `golab hook install` has ever done.
    Install {
        /// The git pre-commit hook that runs `golab check`.
        #[arg(long)]
        git: bool,
        /// `.claude/settings.json` hooks: join, guard, progress, leave.
        #[arg(long)]
        claude_code: bool,
        /// `.mcp.json` entry so the editor launches `golab mcp` itself.
        #[arg(long)]
        mcp: bool,
        /// Pin the agent name written into the config.
        #[arg(long = "as")]
        as_agent: Option<String>,
    },
    Uninstall {
        #[arg(long)]
        git: bool,
        #[arg(long)]
        claude_code: bool,
        #[arg(long)]
        mcp: bool,
    },
    /// Editor callback: decide whether an edit may proceed. Reads the event
    /// payload on stdin. Not for humans.
    #[command(hide = true)]
    Guard {
        /// How to say no. `json` writes a structured decision, `exit` uses the
        /// older exit-2-plus-stderr contract, `both` (default) does each — so
        /// a client that understands neither one still blocks.
        #[arg(long, default_value = "both")]
        mode: String,
    },
    #[command(hide = true)]
    SessionStart,
    #[command(hide = true)]
    PostTool,
    #[command(hide = true)]
    SessionEnd,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let style = if cli.json {
        Style::plain()
    } else {
        Style::detect()
    };
    match run(&cli, &style) {
        Ok(code) => code,
        Err(e) => {
            if cli.json {
                println!("{}", json!({ "error": e.to_string() }));
            } else {
                eprintln!("{} {e:#}", style.red("error:"));
            }
            ExitCode::from(2)
        }
    }
}

/// Exit code 0 = success, 1 = "denied" (a normal answer, not an error),
/// 2 = something went wrong.
fn run(cli: &Cli, st: &Style) -> Result<ExitCode> {
    if let Command::Init { path } = &cli.command {
        let root = path
            .clone()
            .or_else(|| cli.root.clone())
            .unwrap_or(std::env::current_dir()?);
        let root = root.canonicalize().map(strip_verbatim).unwrap_or(root);
        Store::init(&root)?;
        if cli.json {
            emit(&json!({ "root": root, "initialized": true }));
        } else {
            println!(
                "{} runtime ready at {}\n  next: {}",
                st.green("✔"),
                st.bold(&root.join(".golab").display().to_string()),
                st.dim("golab scan && golab agent register <name>")
            );
        }
        return Ok(ExitCode::SUCCESS);
    }

    // Hook callbacks run before anything else opens the store, and must never
    // propagate an error: they fire on the critical path of somebody's
    // keystroke, and a workspace that is missing or briefly locked has to mean
    // "carry on", not "your edit failed". `guard` is the one exception — its
    // whole job is to be able to say no.
    if let Command::Hook(cmd) = &cli.command {
        match cmd {
            HookCmd::Guard { mode } => {
                let Ok(root) = workspace_root(cli) else {
                    return Ok(ExitCode::SUCCESS);
                };
                return hooks::guard(&root, mode);
            }
            HookCmd::SessionStart => {
                return Ok(match workspace_root(cli) {
                    Ok(root) => hooks::session_start(&root).unwrap_or(ExitCode::SUCCESS),
                    Err(_) => ExitCode::SUCCESS,
                })
            }
            HookCmd::PostTool => {
                return Ok(match workspace_root(cli) {
                    Ok(root) => hooks::post_tool(&root).unwrap_or(ExitCode::SUCCESS),
                    Err(_) => ExitCode::SUCCESS,
                })
            }
            HookCmd::SessionEnd => {
                return Ok(match workspace_root(cli) {
                    Ok(root) => hooks::session_end(&root).unwrap_or(ExitCode::SUCCESS),
                    Err(_) => ExitCode::SUCCESS,
                })
            }
            HookCmd::Install { .. } | HookCmd::Uninstall { .. } => {}
        }
    }

    let root = workspace_root(cli)?;
    let mut store = Store::open(&golab_core::db_path(&root))?;

    match &cli.command {
        Command::Init { .. } => unreachable!("handled above"),

        Command::Repo(cmd) => return repo_cmd(cli, st, &mut store, cmd),

        Command::Scan { paths, force } => {
            // A targeted rescan is the same situation the watcher handles —
            // an agent's edit landing while others may be mid-task — so it
            // gets the same proactive notice. `scan_and_notify` already
            // no-ops the notification pass for a full (empty-paths) scan.
            // Every registered repo is scanned when no paths are given;
            // given paths are grouped by whichever repo owns them.
            let stats = scan_workspace(&mut store, &root, paths, *force)?;
            if cli.json {
                emit(&serde_json::to_value(&stats)?);
            } else {
                println!(
                    "{} indexed {} file(s) ({} unchanged, {} removed) → {} symbols, {} edges in {}ms",
                    st.green("✔"),
                    stats.files_indexed,
                    stats.files_unchanged,
                    stats.files_removed,
                    store.symbol_count()?,
                    store.edge_count()?,
                    stats.elapsed_ms
                );
                if stats.leases_dropped > 0 {
                    println!(
                        "  {} {} lease(s) dropped because their symbols disappeared",
                        st.yellow("!"),
                        stats.leases_dropped
                    );
                }
                if stats.files_failed > 0 {
                    println!(
                        "  {} {} file(s) failed to parse",
                        st.dim("·"),
                        stats.files_failed
                    );
                }
            }
        }

        Command::Symbols {
            pattern,
            path,
            kind,
            role,
            limit,
        } => {
            let kind = match kind {
                Some(k) => {
                    Some(SymbolKind::parse(k).ok_or_else(|| anyhow!("unknown symbol kind: {k}"))?)
                }
                None => None,
            };
            let role = match role {
                Some(r) => Some(Role::parse(r).ok_or_else(|| anyhow!("unknown role: {r}"))?),
                None => None,
            };
            let mut symbols = store.list_symbols(path.as_deref(), kind, pattern.as_deref(), *limit)?;
            if let Some(role) = role {
                symbols.retain(|s| s.role == Some(role));
            }
            if cli.json {
                emit(&serde_json::to_value(&symbols)?);
            } else if symbols.is_empty() {
                println!("{}", st.dim("no symbols matched"));
            } else {
                for s in &symbols {
                    println!("{}", render::symbol_line(s, st));
                }
                println!("{}", st.dim(&format!("{} symbol(s)", symbols.len())));
            }
        }

        Command::Show { symbol } => {
            let sym = store.resolve(symbol)?;
            let neighbors = store.neighbors(&sym.id)?;
            let leases: Vec<Lease> = store
                .active_leases(None)?
                .into_iter()
                .filter(|l| l.symbol_id == sym.id)
                .collect();
            if cli.json {
                emit(&json!({
                    "symbol": sym,
                    "leases": leases,
                    "callers": neighbors.callers,
                    "callees": neighbors.callees,
                    "children": neighbors.children,
                }));
            } else {
                println!("{}", render::symbol_line(&sym, st));
                println!("  {} {}", st.dim("id"), sym.id);
                println!(
                    "  {} lines {}-{}",
                    st.dim("span"),
                    sym.start_line + 1,
                    sym.end_line + 1
                );
                let now = golab_core::now_ms();
                if leases.is_empty() {
                    println!("  {} {}", st.dim("lease"), st.green("free"));
                } else {
                    for l in &leases {
                        println!("  {} {}", st.dim("lease"), render::lease_line(l, now, st));
                    }
                }
                print_neighbors("callers", &neighbors.callers, st);
                print_neighbors("callees", &neighbors.callees, st);
                if !neighbors.children.is_empty() {
                    println!("  {}", st.dim("contains"));
                    for c in &neighbors.children {
                        println!("    {}", render::symbol_line(c, st));
                    }
                }
            }
        }

        Command::Index { watch } => {
            if !*watch {
                let stats = scan_workspace(&mut store, &root, &[], false)?;
                if cli.json {
                    emit(&serde_json::to_value(&stats)?);
                } else {
                    println!("{}", render::scan_line(&stats, st));
                }
                return Ok(ExitCode::SUCCESS);
            }
            // Catch up every repo first (--watch itself only follows the
            // default repo for now — see `golab repo` in the README).
            let stats = scan_workspace(&mut store, &root, &[], false)?;
            if !cli.json {
                println!("{}", render::scan_line(&stats, st));
                println!(
                    "{}",
                    st.dim("watching for changes… (ctrl-c to stop)")
                );
            }
            let json = cli.json;
            golab_daemon::watcher::watch(
                &root,
                golab_daemon::watcher::Stop::new(),
                |paths, stats| {
                    if json {
                        let files: Vec<String> =
                            paths.iter().map(|p| p.display().to_string()).collect();
                        println!(
                            "{}",
                            json!({ "reindexed": files, "stats": stats })
                        );
                    } else {
                        println!(
                            "{} {} {}",
                            st.dim(&render::timestamp(golab_core::now_ms())),
                            st.green("↻"),
                            render::scan_line(stats, st)
                        );
                    }
                },
            )?;
        }

        Command::Api { pattern } => {
            let mut endpoints = store.symbols_with_role(Role::Api)?;
            if let Some(p) = pattern {
                endpoints.retain(|s| s.route().unwrap_or_default().contains(p.as_str()));
            }
            if cli.json {
                emit(&serde_json::to_value(&endpoints)?);
            } else if endpoints.is_empty() {
                println!("{}", st.dim("no HTTP endpoints found"));
            } else {
                for e in &endpoints {
                    println!("{}", render::endpoint_line(e, st));
                }
                println!("{}", st.dim(&format!("{} endpoint(s)", endpoints.len())));
            }
        }

        Command::Tests { symbol } => match symbol {
            Some(query) => {
                let sym = store.resolve(query)?;
                let covering: Vec<_> = store
                    .neighbors(&sym.id)?
                    .callers
                    .into_iter()
                    .filter(|n| n.kind == EdgeKind::Tests)
                    .collect();
                if cli.json {
                    emit(&json!({ "symbol": sym, "tests": covering }));
                } else if covering.is_empty() {
                    println!(
                        "{} no test reaches {}",
                        st.yellow("!"),
                        st.bold(&sym.handle())
                    );
                } else {
                    println!("{} is covered by", st.bold(&sym.handle()));
                    for n in &covering {
                        println!("  {}", render::symbol_line(&n.symbol, st));
                    }
                }
                if covering.is_empty() {
                    return Ok(ExitCode::from(1));
                }
            }
            None => {
                let tests = store.symbols_with_role(Role::Test)?;
                if cli.json {
                    emit(&serde_json::to_value(&tests)?);
                } else {
                    for t in &tests {
                        println!("{}", render::symbol_line(t, st));
                    }
                    println!("{}", st.dim(&format!("{} test(s)", tests.len())));
                }
            }
        },

        Command::Tables { name } => {
            let tables = match name {
                Some(n) => vec![store.resolve(n)?],
                None => store.list_symbols(None, Some(SymbolKind::Table), None, 500)?,
            };
            if cli.json {
                let mut out = Vec::new();
                for t in &tables {
                    let users = store.neighbors(&t.id)?.callers;
                    out.push(json!({ "table": t, "accessed_by": users }));
                }
                emit(&serde_json::Value::Array(out));
            } else if tables.is_empty() {
                println!("{}", st.dim("no tables indexed (add .sql schema files)"));
            } else {
                for t in &tables {
                    let users = store.neighbors(&t.id)?.callers;
                    println!(
                        "{:<24} {} {}",
                        st.bold(&t.name),
                        st.dim(&t.location()),
                        st.dim(&format!("{} accessor(s)", users.len()))
                    );
                    for u in users.iter().filter(|u| u.kind == EdgeKind::Queries) {
                        println!("    {}", render::symbol_line(&u.symbol, st));
                    }
                }
            }
        }

        Command::Services => {
            let services = store.list_symbols(None, Some(SymbolKind::Service), None, 200)?;
            if cli.json {
                let mut out = Vec::new();
                for s in &services {
                    let deps: Vec<String> = store
                        .neighbors(&s.id)?
                        .callees
                        .iter()
                        .filter(|n| n.kind == EdgeKind::Imports)
                        .map(|n| n.symbol.name.clone())
                        .collect();
                    out.push(json!({ "service": s, "depends_on": deps }));
                }
                emit(&serde_json::Value::Array(out));
            } else if services.is_empty() {
                println!("{}", st.dim("no manifests found"));
            } else {
                for s in &services {
                    let deps: Vec<String> = store
                        .neighbors(&s.id)?
                        .callees
                        .iter()
                        .filter(|n| n.kind == EdgeKind::Imports)
                        .map(|n| n.symbol.name.clone())
                        .collect();
                    let files = store.children(&s.id)?.len();
                    println!(
                        "{:<20} {:<8} {} {}",
                        st.bold(&s.name),
                        st.dim(s.meta["ecosystem"].as_str().unwrap_or("")),
                        st.dim(&format!("{files} file(s)")),
                        if deps.is_empty() {
                            String::new()
                        } else {
                            st.dim(&format!("→ {}", deps.join(", ")))
                        }
                    );
                }
            }
        }

        Command::Owners { path } => {
            let owners = golab_core::topology::Owners::load(&root);
            let target = match path {
                Some(p) => {
                    // Accept a symbol too: agents think in symbols, not paths.
                    let normalized = p.replace('\\', "/");
                    store
                        .resolve(&normalized)
                        .map(|s| s.path)
                        .unwrap_or(normalized)
                }
                None => {
                    let by_owner = owners.by_owner();
                    if cli.json {
                        emit(&json!({ "source": owners.source, "owners": by_owner }));
                    } else if by_owner.is_empty() {
                        println!("{}", st.dim("no CODEOWNERS file in this repository"));
                    } else {
                        for (owner, patterns) in &by_owner {
                            println!(
                                "{:<24} {}",
                                st.bold(owner),
                                st.dim(&patterns.join(" "))
                            );
                        }
                    }
                    return Ok(ExitCode::SUCCESS);
                }
            };

            let codeowners = owners.owners_of(&target);
            let rule = owners.matching_rule(&target).map(|r| r.pattern.clone());
            let holders: Vec<Lease> = store
                .active_leases(None)?
                .into_iter()
                .filter(|l| l.symbol_handle.starts_with(&target))
                .collect();
            if cli.json {
                emit(&json!({
                    "path": target,
                    "codeowners": codeowners,
                    "rule": rule,
                    "leases": holders,
                }));
            } else {
                println!("{}", st.bold(&target));
                if codeowners.is_empty() {
                    println!("  {} {}", st.dim("codeowners"), st.dim("none"));
                } else {
                    println!(
                        "  {} {} {}",
                        st.dim("codeowners"),
                        codeowners.join(" "),
                        st.dim(&rule.map(|r| format!("(via {r})")).unwrap_or_default())
                    );
                }
                let now = golab_core::now_ms();
                if holders.is_empty() {
                    println!("  {} {}", st.dim("leased"), st.dim("nobody right now"));
                } else {
                    for l in &holders {
                        println!("  {} {}", st.dim("leased"), render::lease_line(l, now, st));
                    }
                }
            }
        }

        Command::Graph { symbol, depth } => {
            let sym = store.resolve(symbol)?;
            let impact = store.impact(&sym.id, *depth)?;
            if cli.json {
                emit(&json!({ "symbol": sym, "impact": impact }));
            } else {
                println!("{} {}", st.bold("impact of changing"), st.bold(&sym.handle()));
                if impact.is_empty() {
                    println!("  {}", st.dim("nothing else references it"));
                }
                for node in &impact {
                    let owner = match &node.lease {
                        Some(l) => st.red(&format!(
                            "  ← leased by {} for {}",
                            l.agent,
                            render::duration(l.seconds_left(golab_core::now_ms()))
                        )),
                        None => String::new(),
                    };
                    println!(
                        "  {}{} {}{}",
                        st.dim(&"·".repeat(node.distance)),
                        st.dim(&format!("[{}]", node.distance)),
                        st.bold(&node.symbol.handle()),
                        owner
                    );
                }
            }
        }

        Command::Schedule { infer } => {
            store.sweep().ok();
            let inferred = if *infer {
                store.infer_dependencies()?
            } else {
                Vec::new()
            };
            let plan = store.plan()?;
            if cli.json {
                emit(&json!({ "plan": plan, "inferred": inferred }));
            } else {
                if !inferred.is_empty() {
                    println!("{}", st.bold("inferred from the code graph"));
                    for d in &inferred {
                        println!(
                            "  {} {} depends on {} {}",
                            st.dim("+"),
                            st.bold(&d.task),
                            st.bold(&d.depends_on),
                            st.dim(&format!("({} {})", d.via, d.edge.as_str()))
                        );
                    }
                    println!();
                }
                print!("{}", render::plan_block(&plan, st));
            }
        }

        Command::Throughput { minutes } => {
            let t = store.throughput(*minutes, 30)?;
            if cli.json {
                emit(&serde_json::to_value(&t)?);
            } else {
                print!("{}", render::throughput_block(&t, st));
            }
        }

        Command::Agent(cmd) => return agent_cmd(cli, st, &mut store, &root, cmd),
        Command::Lease(cmd) => return lease_cmd(cli, st, &mut store, &root, cmd),
        Command::Task(cmd) => return task_cmd(cli, st, &mut store, &root, cmd),
        Command::Memory(cmd) => return memory_cmd(cli, st, &mut store, &root, cmd),
        Command::Msg(cmd) => return msg_cmd(cli, st, &mut store, &root, cmd),
        Command::Request(cmd) => return request_cmd(cli, st, &mut store, &root, cmd),
        Command::Hook(cmd) => return hook_cmd(cli, st, &root, cmd),
        Command::Goal(cmd) => return goal_cmd(cli, st, &mut store, &root, cmd),
        Command::Swarm(cmd) => return swarm_cmd(cli, st, &mut store, &root, cmd),
        Command::Review(cmd) => return review_cmd(cli, st, &mut store, &root, cmd),

        Command::Assign {
            id,
            to,
            preempt,
            priority,
        } => {
            let actor = optional_agent(cli, &root);
            // A goal id picks its own next startable task; a task id is
            // assigned directly.
            let target = if id.starts_with('G') && store.goal(id)?.is_some() {
                let ready = store
                    .goal_tasks(id)?
                    .into_iter()
                    .find(|t| t.ready)
                    .ok_or_else(|| anyhow!("no startable task under {id}"))?;
                ready.task.id
            } else {
                id.clone()
            };
            let t = store.assign_task_opts(&target, to, actor.as_deref(), *preempt, *priority)?;
            if cli.json {
                emit(&serde_json::to_value(&t)?);
            } else {
                println!(
                    "{} {} assigned to {}",
                    st.green("✔"),
                    st.bold(&target),
                    st.bold(to)
                );
                println!("{}", render::task_line(&t, st));
            }
        }

        Command::Observe { goal } => {
            store.sweep().ok();
            let agents = store.agents()?;
            let tasks = match goal {
                Some(g) => store.goal_tasks(g)?,
                None => store.tasks()?,
            };
            let plan = store.plan()?;
            if cli.json {
                emit(&json!({ "agents": agents, "tasks": tasks, "plan": plan }));
            } else {
                print!("{}", render::observe_block(&agents, &tasks, &plan, st));
            }
        }

        Command::Continue { ttl, goal } => {
            let agent = require_agent(cli, &root)?;
            // Already holding work: renew it rather than reaching for
            // something new — "continue" means keep going, not start over.
            let mine = store.agents()?.into_iter().find(|a| a.agent.name == agent);
            if let Some(a) = mine.filter(|a| a.current_task.is_some()) {
                let renewed = store.heartbeat(&agent, None)?;
                if cli.json {
                    emit(&json!({ "task": a.current_task, "renewed": renewed }));
                } else {
                    println!(
                        "{} still on {} {}",
                        st.green("▶"),
                        st.bold(a.current_task.as_deref().unwrap_or("?")),
                        st.dim(&format!("({} lease(s) renewed)", renewed.len()))
                    );
                }
                return Ok(ExitCode::SUCCESS);
            }
            match store.claim_next_in(&agent, *ttl, goal.as_deref())? {
                Some(t) => {
                    if cli.json {
                        emit(&serde_json::to_value(&t)?);
                    } else {
                        println!("{}", render::scheduled_line(&t, st));
                    }
                }
                None => {
                    if cli.json {
                        emit(&json!({ "task": null }));
                    } else {
                        println!(
                            "{}",
                            st.dim(&match goal {
                                Some(g) => format!("nothing under {g} you can start right now"),
                                None => "nothing is ready — every task is blocked, taken, held \
                                         by someone else, or done"
                                    .to_string(),
                            })
                        );
                    }
                    return Ok(ExitCode::from(1));
                }
            }
        }

        Command::Progress {
            percent,
            note,
            eta,
            task,
            symbol,
        } => {
            let agent = require_agent(cli, &root)?;
            let symbol_id = match symbol {
                Some(s) => Some(store.resolve(s)?.id),
                None => None,
            };
            let update = store.record_progress(
                &agent,
                task.as_deref(),
                symbol_id.as_deref(),
                *percent,
                *eta,
                note.as_deref(),
            )?;
            if cli.json {
                emit(&serde_json::to_value(&update)?);
            } else {
                println!("{} {}", st.green("✔"), render::progress_line(&update, st));
            }
        }

        Command::Check { paths, warn_only } => {
            let agent = require_agent(cli, &root)?;
            let report = check_workspace(&store, &root, &agent, paths)?;
            if cli.json {
                emit(&serde_json::to_value(&report)?);
            } else {
                print!("{}", render::check_block(&report, st));
            }
            if !report.ok() && !warn_only {
                return Ok(ExitCode::from(1));
            }
        }

        Command::Guard {
            path,
            symbol,
            strict,
        } => {
            let agent = require_agent(cli, &root)?;
            let report = guard_workspace(&store, &root, &agent, path, symbol.as_deref())?;
            if cli.json {
                emit(&serde_json::to_value(&report)?);
            } else {
                print!("{}", render::guard_block(&report, st));
            }
            // A warning is not a denial — an agent editing an unleased,
            // uncontended symbol is doing something legal, just unprotected.
            // `--strict` is for callers that want the stronger rule.
            let refused = report.blocking() || (*strict && report.verdict != GuardVerdict::Allowed);
            if refused {
                return Ok(ExitCode::from(1));
            }
        }

        Command::Context { task, agent, depth } => {
            // A task id if given, else the named agent, else whoever is
            // acting — the same question asked three ways.
            let task_id = task.clone();
            let who = agent
                .clone()
                .or_else(|| optional_agent(cli, &root))
                .filter(|_| task_id.is_none());

            match (task_id, who) {
                (Some(t), _) => {
                    let ctx = store.task_context(&t, *depth)?;
                    if cli.json {
                        emit(&serde_json::to_value(&ctx)?);
                    } else {
                        print!("{}", render::task_context_block(&ctx, st));
                    }
                }
                (None, Some(a)) => {
                    let ctx = store.agent_context(&a, *depth)?;
                    if cli.json {
                        emit(&serde_json::to_value(&ctx)?);
                    } else {
                        print!("{}", render::agent_context_block(&ctx, st));
                    }
                }
                (None, None) => bail!(
                    "nothing to orient — pass --task, pass --agent, or set an agent identity"
                ),
            }
        }

        Command::Session(cmd) => return session_cmd(cli, st, &mut store, cmd),

        Command::Arch { depth, repo, node } => {
            // Sweep first: the overlays are the point, and a stale "alice is
            // editing this" makes the picture worse than no picture.
            store.sweep().ok();
            match node {
                Some(id) => {
                    let detail = store.arch_node(id, *depth)?;
                    if cli.json {
                        emit(&serde_json::to_value(&detail)?);
                    } else {
                        print!("{}", render::arch_node_block(&detail, st));
                    }
                }
                None => {
                    let graph = store.architecture(repo.as_deref(), *depth)?;
                    if cli.json {
                        emit(&serde_json::to_value(&graph)?);
                    } else {
                        print!("{}", render::arch_block(&graph, st));
                    }
                }
            }
        }

        Command::Activity { all, agent } => {
            // Sweep first, for the same reason `session list` does: a tool that
            // stopped two minutes ago must not still read as mid-edit.
            store.sweep().ok();
            let rows = match (agent, all) {
                (Some(a), _) => store.activity_for_agent(a)?,
                (None, true) => store.all_activity(100)?,
                (None, false) => store.live_activity()?,
            };
            let rows: Vec<_> = if *all {
                rows
            } else {
                rows.into_iter().filter(|r| r.live).collect()
            };
            if cli.json {
                emit(&serde_json::to_value(&rows)?);
            } else if rows.is_empty() {
                println!("{}", st.dim("nobody is editing anything right now"));
            } else {
                for r in &rows {
                    println!("{}", render::activity_line(r, st));
                }
            }
        }

        Command::Diff { paths } => {
            let (changes, unparsed) = diff_workspace(&store, &root, paths)?;
            if cli.json {
                emit(&json!({ "changes": changes, "unparsed": unparsed }));
            } else if changes.is_empty() {
                println!("{} working tree matches the index", st.green("✔"));
            } else {
                for c in &changes {
                    let marker = match c.change {
                        ChangeKind::Added => st.green("+"),
                        ChangeKind::Removed => st.red("-"),
                        ChangeKind::Modified => st.yellow("~"),
                    };
                    println!(
                        "{} {:<10} {}",
                        marker,
                        st.dim(c.kind.as_str()),
                        st.bold(&c.handle)
                    );
                }
                println!("{}", st.dim(&format!("{} change(s)", changes.len())));
            }
        }

        Command::Watch { since, once, tail } => {
            // One line per event, in whichever language the caller asked for.
            // The follow loop below has always honoured `--json`; the backlog
            // and `--once` paths used to print human text regardless, so
            // `golab --json watch` emitted a header of prose and then started
            // streaming JSON.
            let show = |e: &golab_core::model::Event| -> Result<()> {
                if cli.json {
                    println!("{}", serde_json::to_string(e)?);
                } else {
                    println!("{}", render::event_line(e, st));
                }
                Ok(())
            };
            let mut cursor = if *since > 0 {
                *since
            } else {
                let last = store.last_event_id()?;
                for e in &store.recent_events(*tail)? {
                    show(e)?;
                }
                last
            };
            if *once {
                for e in store.events_since(cursor, 500)? {
                    show(&e)?;
                }
                return Ok(ExitCode::SUCCESS);
            }
            if !cli.json {
                println!("{}", st.dim("watching… (ctrl-c to stop)"));
            }
            loop {
                // Expiry is lazy, and a watcher is exactly the process that
                // should notice a dead agent's lease lapse.
                store.sweep().ok();
                for e in store.events_since(cursor, 500)? {
                    cursor = e.id;
                    if cli.json {
                        println!("{}", serde_json::to_string(&e)?);
                    } else {
                        println!("{}", render::event_line(&e, st));
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(400));
            }
        }

        Command::Status { events } => {
            store.sweep().ok();
            let summary = store.status(*events)?;
            if cli.json {
                emit(&serde_json::to_value(&summary)?);
            } else {
                print!("{}", render::status_block(&summary, st));
            }
        }

        Command::Serve {
            port,
            host,
            no_watch,
        } => {
            let addr = format!("{host}:{port}");
            if !cli.json {
                println!(
                    "{} dashboard on {}{}",
                    st.green("▶"),
                    st.bold(&format!("http://{addr}")),
                    if *no_watch {
                        String::new()
                    } else {
                        st.dim(" · watching for changes")
                    }
                );
            }
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;
            rt.block_on(golab_daemon::serve(root.clone(), &addr, !*no_watch))?;
        }

        Command::Mcp {
            as_agent,
            tool,
            heartbeat_secs,
            keep_leases,
        } => {
            // Nothing may be printed to stdout from here on: it is the
            // protocol channel until the client closes stdin. That includes
            // the error path — `main` reports failures as JSON on *stdout*
            // under `--json`, which would be a malformed frame, so this arm
            // handles its own errors and never propagates.
            let outcome = golab_mcp::serve(golab_mcp::McpConfig {
                agent: as_agent.clone().or_else(|| cli.agent.clone()),
                tool: tool.clone(),
                heartbeat_secs: *heartbeat_secs,
                keep_leases: *keep_leases,
                ..golab_mcp::McpConfig::new(root.clone())
            });
            if let Err(e) = outcome {
                eprintln!("golab mcp: {e:#}");
                return Ok(ExitCode::from(2));
            }
        }
    }

    Ok(ExitCode::SUCCESS)
}

fn print_neighbors(label: &str, items: &[golab_core::graph::Neighbor], st: &Style) {
    if items.is_empty() {
        return;
    }
    println!("  {}", st.dim(label));
    for n in items {
        println!(
            "    {} {}",
            st.dim(n.kind.as_str()),
            st.bold(&n.symbol.handle())
        );
    }
}

fn agent_cmd(
    cli: &Cli,
    st: &Style,
    store: &mut Store,
    root: &Path,
    cmd: &AgentCmd,
) -> Result<ExitCode> {
    match cmd {
        AgentCmd::Register { name, kind, capability } => {
            let mut agent = store.register_agent(name, kind)?;
            if let Some(cap) = capability {
                let cap = Capability::parse(cap).ok_or_else(|| anyhow!("unknown capability: {cap}"))?;
                agent = store.set_agent_capability(name, Some(cap))?;
            }
            // Remember the identity so later commands need no --agent.
            std::fs::write(root.join(golab_core::RUNTIME_DIR).join("agent"), name).ok();
            if cli.json {
                emit(&serde_json::to_value(&agent)?);
            } else {
                println!(
                    "{} {} joined as {} {}",
                    st.green("✔"),
                    st.bold(name),
                    kind,
                    st.dim("(default agent for this workspace)")
                );
            }
        }
        AgentCmd::List => {
            let agents = store.agents()?;
            if cli.json {
                emit(&serde_json::to_value(&agents)?);
            } else if agents.is_empty() {
                println!("{}", st.dim("no agents registered"));
            } else {
                for a in &agents {
                    println!("{}", render::agent_line(a, st));
                }
            }
        }
        AgentCmd::Pause { name } | AgentCmd::Resume { name } => {
            let paused = matches!(cmd, AgentCmd::Pause { .. });
            let who = match name {
                Some(n) => n.clone(),
                None => require_agent(cli, root)?,
            };
            let agent = store.set_agent_paused(&who, paused)?;
            if cli.json {
                emit(&serde_json::to_value(&agent)?);
            } else if paused {
                println!(
                    "{} {} paused {}",
                    st.yellow("‖"),
                    st.bold(&who),
                    st.dim("(keeps its leases, gets no new work)")
                );
            } else {
                println!("{} {} resumed", st.green("▶"), st.bold(&who));
            }
        }
        AgentCmd::Leave { name } => {
            let who = match name {
                Some(n) => n.clone(),
                None => require_agent(cli, root)?,
            };
            let released = store.deregister_agent(&who)?;
            if cli.json {
                emit(&json!({ "agent": who, "leases_released": released }));
            } else {
                println!(
                    "{} {} left, {} lease(s) released",
                    st.green("✔"),
                    st.bold(&who),
                    released
                );
            }
        }
        AgentCmd::Heartbeat => {
            let who = require_agent(cli, root)?;
            let leases = store.heartbeat(&who, None)?;
            if cli.json {
                emit(&serde_json::to_value(&leases)?);
            } else {
                println!(
                    "{} {} lease(s) renewed for {}",
                    st.green("✔"),
                    leases.len(),
                    st.bold(&who)
                );
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn session_cmd(cli: &Cli, st: &Style, store: &mut Store, cmd: &SessionCmd) -> Result<ExitCode> {
    match cmd {
        SessionCmd::List { live } => {
            // Sweep first, or a tool that died two minutes ago still reads as
            // attached — the same reason `lease list` sweeps.
            store.sweep().ok();
            let sessions = store.sessions(*live)?;
            if cli.json {
                emit(&serde_json::to_value(&sessions)?);
            } else if sessions.is_empty() {
                println!("{}", st.dim("no coding tools connected"));
            } else {
                for s in &sessions {
                    println!("{}", render::session_line(s, st));
                }
            }
        }
        SessionCmd::End { id, keep_leases } => {
            let ended = store.end_session(id, !*keep_leases)?;
            if cli.json {
                emit(&serde_json::to_value(&ended)?);
            } else {
                println!(
                    "{} {} disconnected {}",
                    st.green("✔"),
                    st.bold(&ended.agent),
                    st.dim(&format!("({} via {})", ended.tool, ended.transport))
                );
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn lease_cmd(
    cli: &Cli,
    st: &Style,
    store: &mut Store,
    root: &Path,
    cmd: &LeaseCmd,
) -> Result<ExitCode> {
    match cmd {
        LeaseCmd::Acquire {
            symbol,
            ttl,
            priority,
            task,
            note,
            wait,
            preempt,
            no_queue,
        } => {
            let agent = require_agent(cli, root)?;
            let sym = store.resolve(symbol)?;
            let opts = AcquireOptions {
                task: task.clone(),
                ttl_secs: *ttl,
                priority: *priority,
                note: note.clone(),
                preempt: *preempt,
                queue: !*no_queue,
            };

            let deadline =
                wait.map(|w| std::time::Instant::now() + std::time::Duration::from_secs(w));
            let outcome = loop {
                let outcome = store.acquire(&sym.id, &agent, &opts)?;
                if outcome.is_granted() {
                    break outcome;
                }
                match deadline {
                    Some(d) if std::time::Instant::now() < d => {
                        if !cli.json {
                            if let AcquireOutcome::Denied { conflicts } = &outcome {
                                if let Some(c) = conflicts.first() {
                                    eprintln!(
                                        "{} waiting on {} (held by {})",
                                        st.dim("…"),
                                        c.blocking_symbol,
                                        c.holder
                                    );
                                }
                            }
                        }
                        std::thread::sleep(std::time::Duration::from_millis(500));
                    }
                    _ => break outcome,
                }
            };

            if cli.json {
                emit(&serde_json::to_value(&outcome)?);
            }
            match &outcome {
                AcquireOutcome::Granted { lease } | AcquireOutcome::Extended { lease } => {
                    if !cli.json {
                        let verb = if matches!(outcome, AcquireOutcome::Extended { .. }) {
                            "extended"
                        } else {
                            "acquired"
                        };
                        println!(
                            "{} {} {} for {} {}",
                            st.green("✔"),
                            verb,
                            st.bold(&lease.symbol_handle),
                            st.bold(&agent),
                            st.dim(&format!(
                                "(ttl {}, lease {})",
                                render::duration(lease.ttl_secs),
                                lease.id
                            ))
                        );
                    }
                    return Ok(ExitCode::SUCCESS);
                }
                AcquireOutcome::Denied { conflicts } => {
                    if !cli.json {
                        println!("{} cannot lease {}", st.red("✗"), st.bold(&sym.handle()));
                        print!("{}", render::conflict_lines(conflicts, st));
                    }
                    return Ok(ExitCode::from(1));
                }
            }
        }

        LeaseCmd::Release { target, all } => {
            let agent = require_agent(cli, root)?;
            if *all {
                let released = store.release_all(&agent)?;
                if cli.json {
                    emit(&serde_json::to_value(&released)?);
                } else {
                    println!("{} released {} lease(s)", st.green("✔"), released.len());
                }
                return Ok(ExitCode::SUCCESS);
            }
            let target = target
                .as_deref()
                .ok_or_else(|| anyhow!("pass a lease id, a symbol, or --all"))?;
            let lease_id = resolve_lease(store, target, &agent)?;
            let lease = store.release(&lease_id, &agent)?;
            if cli.json {
                emit(&serde_json::to_value(&lease)?);
            } else {
                println!(
                    "{} released {}",
                    st.green("✔"),
                    st.bold(&lease.symbol_handle)
                );
            }
        }

        LeaseCmd::Renew { lease } => {
            let agent = require_agent(cli, root)?;
            let renewed = store.heartbeat(&agent, lease.as_deref())?;
            if cli.json {
                emit(&serde_json::to_value(&renewed)?);
            } else {
                let now = golab_core::now_ms();
                for l in &renewed {
                    println!("{} {}", st.green("✔"), render::lease_line(l, now, st));
                }
                if renewed.is_empty() {
                    println!("{}", st.dim("no active leases to renew"));
                }
            }
        }

        LeaseCmd::List { mine } => {
            store.sweep().ok();
            let agent = if *mine {
                Some(require_agent(cli, root)?)
            } else {
                None
            };
            let leases = store.active_leases(agent.as_deref())?;
            if cli.json {
                emit(&serde_json::to_value(&leases)?);
            } else if leases.is_empty() {
                println!("{}", st.dim("no active leases"));
            } else {
                let now = golab_core::now_ms();
                for l in &leases {
                    println!("{}", render::lease_line(l, now, st));
                }
            }
        }

        LeaseCmd::Transfer { target, to } => {
            let agent = require_agent(cli, root)?;
            let lease_id = resolve_lease(store, target, &agent)?;
            let lease = store.transfer_lease(&lease_id, &agent, to)?;
            if cli.json {
                emit(&serde_json::to_value(&lease)?);
            } else {
                println!(
                    "{} {} now belongs to {} {}",
                    st.green("✔"),
                    st.bold(&lease.symbol_handle),
                    st.bold(to),
                    st.dim(&format!("(lease {} kept alive throughout)", lease.id))
                );
            }
        }

        LeaseCmd::Queue { symbol } => {
            let sym = store.resolve(symbol)?;
            let queue = store.queue_for(&sym.id)?;
            if cli.json {
                emit(&serde_json::to_value(&queue)?);
            } else if queue.is_empty() {
                println!("{}", st.dim("nobody is waiting"));
            } else {
                for (i, q) in queue.iter().enumerate() {
                    println!(
                        "{}. {} {}",
                        i + 1,
                        st.bold(&q.agent),
                        st.dim(&format!(
                            "priority {}{}",
                            q.priority,
                            q.task
                                .as_deref()
                                .map(|t| format!(", task={t}"))
                                .unwrap_or_default()
                        ))
                    );
                }
            }
        }

        LeaseCmd::Check { symbol } => {
            let agent = require_agent(cli, root)?;
            let sym = store.resolve(symbol)?;
            let conflicts = store.conflicts_for(&sym.id, &agent)?;
            if cli.json {
                emit(&json!({ "symbol": sym, "conflicts": conflicts }));
            } else if conflicts.is_empty() {
                println!("{} {} is free", st.green("✔"), st.bold(&sym.handle()));
            } else {
                println!("{} {} is contended", st.yellow("!"), st.bold(&sym.handle()));
                print!("{}", render::conflict_lines(&conflicts, st));
            }
            if !conflicts.is_empty() {
                return Ok(ExitCode::from(1));
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn task_cmd(
    cli: &Cli,
    st: &Style,
    store: &mut Store,
    root: &Path,
    cmd: &TaskCmd,
) -> Result<ExitCode> {
    match cmd {
        TaskCmd::Add {
            title,
            priority,
            deps,
            symbols,
            capability,
        } => {
            let mut task = store.add_task(title, *priority, deps)?;
            let scope = if symbols.is_empty() {
                Vec::new()
            } else {
                store.set_task_scope(&task.id, symbols)?
            };
            if let Some(cap) = capability {
                let cap = Capability::parse(cap).ok_or_else(|| anyhow!("unknown capability: {cap}"))?;
                task = store.set_task_required_capability(&task.id, Some(cap))?.task;
            }
            if cli.json {
                emit(&json!({ "task": task, "scope": scope }));
            } else {
                println!(
                    "{} {} {} {}",
                    st.green("✔"),
                    st.bold(&task.id),
                    task.title,
                    st.dim(&render::scope_summary(&scope))
                );
            }
        }
        TaskCmd::Scope { id, symbols } => {
            let scope = store.set_task_scope(id, symbols)?;
            if cli.json {
                emit(&json!({ "task": id, "scope": scope }));
            } else {
                println!(
                    "{} {} will touch {}",
                    st.green("✔"),
                    st.bold(id),
                    scope
                        .iter()
                        .map(|s| s.handle())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
        }
        TaskCmd::List => {
            let tasks = store.tasks()?;
            if cli.json {
                emit(&serde_json::to_value(&tasks)?);
            } else if tasks.is_empty() {
                println!("{}", st.dim("no tasks"));
            } else {
                for t in &tasks {
                    println!("{}", render::task_line(t, st));
                }
            }
        }
        TaskCmd::Assign { id, to } => {
            let actor = optional_agent(cli, root);
            let t = store.reassign_task(id, to, actor.as_deref())?;
            if cli.json {
                emit(&serde_json::to_value(&t)?);
            } else {
                println!(
                    "{} {} reassigned to {}",
                    st.green("✔"),
                    st.bold(id),
                    st.bold(to)
                );
                println!("{}", render::task_line(&t, st));
            }
        }
        TaskCmd::Priority { id, priority } => {
            let t = store.set_task_priority(id, *priority)?;
            report_task(cli, st, &t);
        }
        TaskCmd::Next { ttl } => {
            let agent = require_agent(cli, root)?;
            return claim_and_report(cli, st, store, &agent, *ttl);
        }
        TaskCmd::Done { id, next, force } => {
            let agent = optional_agent(cli, root);
            // The core releases the task's leases as part of finishing it.
            let t = store.set_task_state(id, TaskState::Done, agent.as_deref(), None, *force)?;
            if !*next {
                report_task(cli, st, &t);
                return Ok(ExitCode::SUCCESS);
            }
            if !cli.json {
                println!("{}", render::task_line(&t, st));
            }
            let agent = require_agent(cli, root)?;
            return claim_and_report(cli, st, store, &agent, DEFAULT_TTL_SECS);
        }
        TaskCmd::Block { id, reason } => {
            let agent = optional_agent(cli, root);
            let t = store.set_task_state(
                id,
                TaskState::Blocked,
                agent.as_deref(),
                reason.as_deref(),
                false,
            )?;
            report_task(cli, st, &t);
        }
        TaskCmd::Fail { id, reason, force } => {
            let agent = optional_agent(cli, root);
            let t = store.set_task_state(
                id,
                TaskState::Failed,
                agent.as_deref(),
                reason.as_deref(),
                *force,
            )?;
            report_task(cli, st, &t);
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn repo_cmd(cli: &Cli, st: &Style, store: &mut Store, cmd: &RepoCmd) -> Result<ExitCode> {
    match cmd {
        RepoCmd::Add { path, name } => {
            let repo = store.add_repo(path, name.as_deref())?;
            if cli.json {
                emit(&serde_json::to_value(&repo)?);
            } else {
                println!(
                    "{} {} {} {}",
                    st.green("✔"),
                    st.bold(&repo.id),
                    repo.name,
                    st.dim(&repo.root_path)
                );
            }
        }
        RepoCmd::List => {
            let repos = store.repos()?;
            if cli.json {
                emit(&serde_json::to_value(&repos)?);
            } else {
                for r in &repos {
                    println!("{:<4} {:<20} {}", st.bold(&r.id), r.name, st.dim(&r.root_path));
                }
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn goal_cmd(cli: &Cli, st: &Style, store: &mut Store, root: &Path, cmd: &GoalCmd) -> Result<ExitCode> {
    match cmd {
        GoalCmd::Add {
            title,
            priority,
            description,
        } => {
            let author = optional_agent(cli, root);
            let goal = store.add_goal(title, *priority, description.as_deref(), author.as_deref())?;
            if cli.json {
                emit(&serde_json::to_value(&goal)?);
            } else {
                println!("{} {} {}", st.green("✔"), st.bold(&goal.id), goal.title);
            }
        }
        GoalCmd::List => {
            let goals = store.goals()?;
            if cli.json {
                emit(&serde_json::to_value(&goals)?);
            } else if goals.is_empty() {
                println!("{}", st.dim("no goals"));
            } else {
                for g in &goals {
                    let progress = store.goal_progress(&g.id)?;
                    println!("{}", render::goal_line(g, &progress, st));
                }
            }
        }
        GoalCmd::Show { id } => {
            let goal = store.goal(id)?.ok_or_else(|| anyhow!("no such goal: {id}"))?;
            let progress = store.goal_progress(id)?;
            let tasks = store.goal_tasks(id)?;
            if cli.json {
                emit(&json!({ "goal": goal, "progress": progress, "tasks": tasks }));
            } else {
                print!("{}", render::goal_progress_block(&goal, &progress, &tasks, st));
            }
        }
        GoalCmd::Decompose {
            id,
            task,
            priority,
            deps,
            symbols,
            capability,
        } => {
            let mut t = store.goal_decompose(id, task, *priority, deps, symbols)?;
            if let Some(cap) = capability {
                let cap = Capability::parse(cap).ok_or_else(|| anyhow!("unknown capability: {cap}"))?;
                t = store.set_task_required_capability(&t.id, Some(cap))?.task;
            }
            if cli.json {
                emit(&serde_json::to_value(&t)?);
            } else {
                println!(
                    "{} {} {} {}",
                    st.green("✔"),
                    st.bold(&t.id),
                    t.title,
                    st.dim(&format!("(under {id})"))
                );
            }
        }
        GoalCmd::Suggest {
            id,
            near,
            depth,
            apply,
        } => {
            if store.goal(id)?.is_none() {
                bail!("no such goal: {id}");
            }
            let suggestions = store.suggest_decomposition(near, *depth)?;
            if *apply {
                let mut created = Vec::new();
                for s in &suggestions {
                    created.push(store.goal_decompose(id, &s.title, 0, &[], &s.symbols)?);
                }
                if cli.json {
                    emit(&serde_json::to_value(&created)?);
                } else {
                    for t in &created {
                        println!("{} {} {}", st.green("✔"), st.bold(&t.id), t.title);
                    }
                }
            } else if cli.json {
                emit(&serde_json::to_value(&suggestions)?);
            } else {
                print!("{}", render::suggestion_block(id, &suggestions, st));
            }
        }
        GoalCmd::Plan { id, depth, apply } => {
            let suggestions = store.plan_goal(id, *depth)?;
            if suggestions.is_empty() {
                if cli.json {
                    emit(&serde_json::to_value(&suggestions)?);
                } else {
                    println!(
                        "{}",
                        st.dim(
                            "no obvious entry point found from the goal's title — try \
                             `goal suggest --near <symbol>` or `goal decompose` directly"
                        )
                    );
                }
                return Ok(ExitCode::from(1));
            }
            if *apply {
                let mut created = Vec::new();
                for s in &suggestions {
                    created.push(store.goal_decompose(id, &s.title, 0, &[], &s.symbols)?);
                }
                if cli.json {
                    emit(&serde_json::to_value(&created)?);
                } else {
                    for t in &created {
                        println!("{} {} {}", st.green("✔"), st.bold(&t.id), t.title);
                    }
                }
            } else if cli.json {
                emit(&serde_json::to_value(&suggestions)?);
            } else {
                print!("{}", render::suggestion_block(id, &suggestions, st));
            }
        }
        GoalCmd::Done { id } => {
            let goal = store.set_goal_state(id, GoalState::Done)?;
            if cli.json {
                emit(&serde_json::to_value(&goal)?);
            } else {
                println!("{} {} done", st.green("✔"), st.bold(&goal.id));
            }
        }
        GoalCmd::Abandon { id } => {
            let goal = store.set_goal_state(id, GoalState::Abandoned)?;
            if cli.json {
                emit(&serde_json::to_value(&goal)?);
            } else {
                println!("{} {} abandoned", st.dim("○"), st.bold(&goal.id));
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn swarm_cmd(
    cli: &Cli,
    st: &Style,
    store: &mut Store,
    root: &Path,
    cmd: &SwarmCmd,
) -> Result<ExitCode> {
    match cmd {
        SwarmCmd::Join { name, kind, capability } => {
            let mut agent = store.register_agent(name, kind)?;
            if let Some(cap) = capability {
                let cap = Capability::parse(cap).ok_or_else(|| anyhow!("unknown capability: {cap}"))?;
                agent = store.set_agent_capability(name, Some(cap))?;
            }
            std::fs::write(root.join(golab_core::RUNTIME_DIR).join("agent"), name).ok();
            if cli.json {
                emit(&serde_json::to_value(&agent)?);
            } else {
                println!(
                    "{} {} joined the swarm as {} {}",
                    st.green("✔"),
                    st.bold(name),
                    kind,
                    st.dim("(default agent for this workspace)")
                );
            }
        }
        SwarmCmd::List => {
            let agents = store.agents()?;
            if cli.json {
                emit(&serde_json::to_value(&agents)?);
            } else if agents.is_empty() {
                println!("{}", st.dim("nobody has joined this workspace"));
            } else {
                for a in &agents {
                    println!("{}", render::swarm_line(a, st));
                }
            }
        }
        SwarmCmd::Pause { name } | SwarmCmd::Resume { name } => {
            let paused = matches!(cmd, SwarmCmd::Pause { .. });
            let who = match name {
                Some(n) => n.clone(),
                None => require_agent(cli, root)?,
            };
            let agent = store.set_agent_paused(&who, paused)?;
            if cli.json {
                emit(&serde_json::to_value(&agent)?);
            } else if paused {
                println!(
                    "{} {} paused {}",
                    st.yellow("‖"),
                    st.bold(&who),
                    st.dim("(keeps its leases, gets no new work)")
                );
            } else {
                println!("{} {} resumed", st.green("▶"), st.bold(&who));
            }
        }
        SwarmCmd::Leave { name } => {
            let who = match name {
                Some(n) => n.clone(),
                None => require_agent(cli, root)?,
            };
            let released = store.deregister_agent(&who)?;
            if cli.json {
                emit(&json!({ "agent": who, "leases_released": released }));
            } else {
                println!(
                    "{} {} left the swarm, {} lease(s) released",
                    st.green("✔"),
                    st.bold(&who),
                    released
                );
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn review_cmd(
    cli: &Cli,
    st: &Style,
    store: &mut Store,
    root: &Path,
    cmd: &ReviewCmd,
) -> Result<ExitCode> {
    match cmd {
        ReviewCmd::Submit { id } => {
            let agent = require_agent(cli, root)?;
            let t = store.submit_for_review(id, &agent)?;
            report_task(cli, st, &t);
        }
        ReviewCmd::Approve { id, force } => {
            let agent = require_agent(cli, root)?;
            let t = store.approve_review(id, &agent, *force)?;
            report_task(cli, st, &t);
        }
        ReviewCmd::Reject { id, reason } => {
            let agent = require_agent(cli, root)?;
            let t = store.reject_review(id, &agent, reason.as_deref())?;
            report_task(cli, st, &t);
        }
        ReviewCmd::List => {
            let tasks = store.tasks_in_review()?;
            if cli.json {
                emit(&serde_json::to_value(&tasks)?);
            } else if tasks.is_empty() {
                println!("{}", st.dim("nothing awaiting review"));
            } else {
                for t in &tasks {
                    println!("{}", render::task_line(t, st));
                }
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// Claim the next startable task and print it. Exit 1 when there is nothing
/// an agent can safely begin, so a scheduler loop can stop.
fn claim_and_report(
    cli: &Cli,
    st: &Style,
    store: &mut Store,
    agent: &str,
    ttl: i64,
) -> Result<ExitCode> {
    match store.claim_next(agent, ttl)? {
        Some(t) => {
            if cli.json {
                emit(&serde_json::to_value(&t)?);
            } else {
                println!("{}", render::scheduled_line(&t, st));
            }
            Ok(ExitCode::SUCCESS)
        }
        None => {
            if cli.json {
                emit(&json!({ "task": null }));
            } else {
                let plan = store.plan()?;
                let contended = plan
                    .waves
                    .first()
                    .map(|w| w.tasks.iter().filter(|t| t.contended_by.is_some()).count())
                    .unwrap_or(0);
                println!(
                    "{}",
                    st.dim(&if contended > 0 {
                        format!(
                            "nothing you can start — {contended} ready task(s) are held by other agents"
                        )
                    } else {
                        "nothing is ready — every task is blocked, taken or done".to_string()
                    })
                );
            }
            Ok(ExitCode::from(1))
        }
    }
}

fn report_task(cli: &Cli, st: &Style, t: &golab_core::work::TaskView) {
    if cli.json {
        emit(&serde_json::to_value(t).unwrap_or_else(|_| json!({})));
    } else {
        println!("{}", render::task_line(t, st));
    }
}

fn memory_cmd(
    cli: &Cli,
    st: &Style,
    store: &mut Store,
    root: &Path,
    cmd: &MemoryCmd,
) -> Result<ExitCode> {
    match cmd {
        MemoryCmd::Set { key, value, tags } => {
            let author = optional_agent(cli, root);
            store.memory_set(key, value, author.as_deref(), tags)?;
            if cli.json {
                emit(&json!({ "key": key, "stored": true }));
            } else {
                println!("{} remembered {}", st.green("✔"), st.bold(key));
            }
        }
        MemoryCmd::Get { key } => match store.memory_get(key)? {
            Some(entry) => {
                if cli.json {
                    emit(&serde_json::to_value(&entry)?);
                } else {
                    println!("{}", entry.value);
                }
            }
            None => {
                if cli.json {
                    emit(&json!({ "key": key, "value": null }));
                } else {
                    println!("{}", st.dim("not found"));
                }
                return Ok(ExitCode::from(1));
            }
        },
        MemoryCmd::List { tag } => {
            let entries = store.memory_list(tag.as_deref())?;
            if cli.json {
                emit(&serde_json::to_value(&entries)?);
            } else if entries.is_empty() {
                println!("{}", st.dim("shared memory is empty"));
            } else {
                for e in &entries {
                    println!(
                        "{:<28} {} {}",
                        st.bold(&e.key),
                        first_line(&e.value),
                        st.dim(&e.author.clone().unwrap_or_default())
                    );
                }
            }
        }
        MemoryCmd::Rm { key } => {
            let removed = store.memory_delete(key)?;
            if cli.json {
                emit(&json!({ "key": key, "removed": removed }));
            } else if removed {
                println!("{} forgot {}", st.green("✔"), st.bold(key));
            } else {
                println!("{}", st.dim("not found"));
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn msg_cmd(
    cli: &Cli,
    st: &Style,
    store: &mut Store,
    root: &Path,
    cmd: &MsgCmd,
) -> Result<ExitCode> {
    match cmd {
        MsgCmd::Send {
            to,
            subject,
            body,
            reply_to,
        } => {
            let from = require_agent(cli, root)?;
            let parsed: serde_json::Value =
                serde_json::from_str(body).unwrap_or_else(|_| json!({ "text": body }));
            let id = store.send_message(&from, to, subject, parsed, *reply_to)?;
            if cli.json {
                emit(&json!({ "id": id }));
            } else {
                println!("{} sent #{id} to {}", st.green("✔"), st.bold(to));
            }
        }
        MsgCmd::Inbox { all } => {
            let me = require_agent(cli, root)?;
            let msgs = store.inbox(&me, !*all)?;
            if cli.json {
                emit(&serde_json::to_value(&msgs)?);
            } else if msgs.is_empty() {
                println!("{}", st.dim("inbox empty"));
            } else {
                for m in &msgs {
                    println!(
                        "{} {:<16} {:<20} {}",
                        st.dim(&format!("#{}", m.id)),
                        st.blue(&m.from),
                        st.bold(&m.subject),
                        st.dim(&m.body.to_string())
                    );
                }
            }
        }
        MsgCmd::Read { id } => {
            let me = require_agent(cli, root)?;
            let ok = store.mark_read(*id, &me)?;
            if cli.json {
                emit(&json!({ "id": id, "read": ok }));
            } else if ok {
                println!("{} marked #{id} read", st.green("✔"));
            } else {
                println!("{}", st.dim("no such unread message"));
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn request_cmd(
    cli: &Cli,
    st: &Style,
    store: &mut Store,
    root: &Path,
    cmd: &RequestCmd,
) -> Result<ExitCode> {
    use golab_core::model::request_kind;
    use golab_core::protocol::{Direction, NewRequest};

    match cmd {
        RequestCmd::Lease {
            symbol,
            reason,
            deadline,
            priority,
            task,
            wait,
        } => {
            let agent = require_agent(cli, root)?;
            let sym = store.resolve(symbol)?;
            let req = store.request_lease_transfer(
                &sym.id,
                &agent,
                reason.as_deref(),
                Some(*deadline),
                *priority,
                task.as_deref(),
            )?;
            if !cli.json {
                println!(
                    "{} asked {} for {} {}",
                    st.green("→"),
                    st.bold(req.to.as_deref().unwrap_or("anyone")),
                    st.bold(&sym.handle()),
                    st.dim(&format!("(request {})", req.id))
                );
            }
            match wait {
                Some(secs) => return wait_for_request(cli, st, store, &req.id, *secs),
                None if cli.json => emit(&serde_json::to_value(&req)?),
                None => {}
            }
        }

        RequestCmd::Interface {
            resource,
            to,
            methods,
            deadline,
            priority,
            task,
        } => {
            let agent = require_agent(cli, root)?;
            let req = store.open_request(&NewRequest {
                to: to.clone(),
                body: json!({ "resource": resource, "methods": methods }),
                task: task.clone(),
                priority: *priority,
                deadline_secs: Some(*deadline),
                ..NewRequest::new(
                    request_kind::INTERFACE,
                    &agent,
                    &format!("need interface {resource}"),
                )
            })?;
            report_request(cli, st, &req);
        }

        RequestCmd::Depend {
            on_task,
            to,
            note,
            deadline,
        } => {
            let agent = require_agent(cli, root)?;
            if store.task(on_task)?.is_none() {
                bail!("no such task: {on_task}");
            }
            let req = store.open_request(&NewRequest {
                to: to.clone(),
                body: json!({ "note": note }),
                resource_task: Some(on_task.clone()),
                deadline_secs: Some(*deadline),
                ..NewRequest::new(
                    request_kind::DEPENDENCY,
                    &agent,
                    &format!("blocked on {on_task}"),
                )
            })?;
            report_request(cli, st, &req);
        }

        RequestCmd::Open {
            kind,
            to,
            subject,
            body,
            symbol,
            about_task,
            task,
            priority,
            deadline,
        } => {
            let agent = require_agent(cli, root)?;
            let symbol_id = match symbol {
                Some(s) => Some(store.resolve(s)?.id),
                None => None,
            };
            let req = store.open_request(&NewRequest {
                to: to.clone(),
                body: parse_body(body),
                resource_symbol: symbol_id,
                resource_task: about_task.clone(),
                task: task.clone(),
                priority: *priority,
                deadline_secs: *deadline,
                ..NewRequest::new(kind, &agent, subject)
            })?;
            report_request(cli, st, &req);
        }

        RequestCmd::Inbox { all } | RequestCmd::Outbox { all } | RequestCmd::List { all } => {
            store.expire_requests().ok();
            let direction = match cmd {
                RequestCmd::Inbox { .. } => Direction::Inbox,
                RequestCmd::Outbox { .. } => Direction::Outbox,
                _ => Direction::All,
            };
            let agent = match direction {
                Direction::All => optional_agent(cli, root),
                _ => Some(require_agent(cli, root)?),
            };
            let requests = store.requests(agent.as_deref(), direction, !*all)?;
            if cli.json {
                emit(&serde_json::to_value(&requests)?);
            } else if requests.is_empty() {
                println!("{}", st.dim("nothing pending"));
            } else {
                for r in &requests {
                    println!("{}", render::request_line(r, st));
                }
            }
        }

        RequestCmd::Show { id } => {
            let req = store
                .request(id)?
                .ok_or_else(|| anyhow!("no such request: {id}"))?;
            if cli.json {
                emit(&serde_json::to_value(&req)?);
            } else {
                print!("{}", render::request_block(&req, st));
            }
        }

        RequestCmd::Accept { id, body } => {
            let agent = require_agent(cli, root)?;
            let req = store.accept_request(id, &agent, Some(parse_body(body)))?;
            if cli.json {
                emit(&serde_json::to_value(&req)?);
            } else if req.kind == request_kind::LEASE_TRANSFER {
                println!(
                    "{} handed {} to {}",
                    st.green("✔"),
                    st.bold(req.resource_handle.as_deref().unwrap_or("the symbol")),
                    st.bold(&req.from)
                );
            } else {
                println!("{} accepted {}", st.green("✔"), st.bold(&req.subject));
            }
        }

        RequestCmd::Decline { id, reason } => {
            let agent = require_agent(cli, root)?;
            let req = store.decline_request(id, &agent, reason.as_deref())?;
            if cli.json {
                emit(&serde_json::to_value(&req)?);
            } else {
                println!(
                    "{} declined {} {}",
                    st.yellow("✗"),
                    st.bold(&req.subject),
                    st.dim(reason.as_deref().unwrap_or(""))
                );
            }
        }

        RequestCmd::Fulfill { id, body } => {
            let agent = require_agent(cli, root)?;
            let req = store.fulfill_request(id, &agent, Some(parse_body(body)))?;
            if cli.json {
                emit(&serde_json::to_value(&req)?);
            } else {
                println!(
                    "{} delivered {} to {}",
                    st.green("✔"),
                    st.bold(&req.subject),
                    st.bold(&req.from)
                );
            }
        }

        RequestCmd::Cancel { id } => {
            let agent = require_agent(cli, root)?;
            let req = store.cancel_request(id, &agent)?;
            if cli.json {
                emit(&serde_json::to_value(&req)?);
            } else {
                println!("{} withdrew {}", st.dim("○"), st.bold(&req.subject));
            }
        }

        RequestCmd::Wait { id, timeout } => {
            return wait_for_request(cli, st, store, id, *timeout);
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// Poll until a request stops being live. Exit 0 only if it was fulfilled —
/// an agent script can branch on that directly.
fn wait_for_request(
    cli: &Cli,
    st: &Style,
    store: &mut Store,
    id: &str,
    timeout_secs: u64,
) -> Result<ExitCode> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    loop {
        store.expire_requests().ok();
        let req = store
            .request(id)?
            .ok_or_else(|| anyhow!("no such request: {id}"))?;
        if !req.state.is_live() {
            if cli.json {
                emit(&serde_json::to_value(&req)?);
            } else {
                print!("{}", render::request_block(&req, st));
            }
            return Ok(if req.state == RequestState::Fulfilled {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(1)
            });
        }
        if std::time::Instant::now() >= deadline {
            if cli.json {
                emit(&json!({ "request": req, "timed_out": true }));
            } else {
                println!(
                    "{} still {} after {timeout_secs}s",
                    st.yellow("…"),
                    req.state.as_str()
                );
            }
            return Ok(ExitCode::from(1));
        }
        std::thread::sleep(std::time::Duration::from_millis(400));
    }
}

fn report_request(cli: &Cli, st: &Style, req: &Request) {
    if cli.json {
        emit(&serde_json::to_value(req).unwrap_or_else(|_| json!({})));
    } else {
        println!("{} {}", st.green("→"), render::request_line(req, st));
    }
}

/// Requests carry JSON payloads, but a human typing a sentence should not have
/// to quote it.
fn parse_body(raw: &str) -> serde_json::Value {
    serde_json::from_str(raw).unwrap_or_else(|_| json!({ "text": raw }))
}

/// Accept either a lease id or a symbol reference the agent holds.
fn resolve_lease(store: &Store, target: &str, agent: &str) -> Result<String> {
    if let Some(l) = store.lease(target)? {
        return Ok(l.id);
    }
    let sym = store.resolve(target)?;
    store
        .active_leases(Some(agent))?
        .into_iter()
        .find(|l| l.symbol_id == sym.id)
        .map(|l| l.id)
        .ok_or_else(|| anyhow!("{agent} holds no lease on {}", sym.handle()))
}

/// The absolute path of the running binary, forward-slashed.
///
/// Hook processes and MCP launches both start with a minimal environment where
/// `golab` is usually not on PATH, so a config that just says `golab` fails
/// for the wrong reason.
fn own_exe() -> String {
    std::env::current_exe()
        .map(|p| strip_verbatim(p).display().to_string().replace('\\', "/"))
        .unwrap_or_else(|_| "golab".to_string())
}

/// The tail we recognise our own entries by on uninstall. Matching the whole
/// command would fail for anyone who rebuilt or moved the binary since.
const HOOK_MARKER: &str = " hook ";

fn hook_cmd(cli: &Cli, st: &Style, root: &Path, cmd: &HookCmd) -> Result<ExitCode> {
    match cmd {
        HookCmd::Install {
            git,
            claude_code,
            mcp,
            as_agent,
        } => {
            // Bare `golab hook install` has always meant the git hook, and
            // still does.
            let git = *git || !(*claude_code || *mcp);
            let mut done: Vec<serde_json::Value> = Vec::new();
            if git {
                done.push(install_git_hook(root)?);
            }
            if *claude_code {
                done.push(install_claude_hooks(root)?);
            }
            if *mcp {
                done.push(install_mcp_server(root, as_agent.as_deref())?);
            }
            if cli.json {
                emit(&json!({ "installed": done }));
            } else {
                for d in &done {
                    println!(
                        "{} {} {}",
                        st.green("✔"),
                        d["what"].as_str().unwrap_or(""),
                        st.dim(d["path"].as_str().unwrap_or(""))
                    );
                }
                if *claude_code {
                    println!(
                        "  {}",
                        st.dim("restart your editor for the hooks to take effect")
                    );
                }
            }
        }
        HookCmd::Uninstall {
            git,
            claude_code,
            mcp,
        } => {
            let git = *git || !(*claude_code || *mcp);
            let mut done: Vec<serde_json::Value> = Vec::new();
            if git {
                let path = root.join(".git").join("hooks").join("pre-commit");
                let existed = path.exists();
                if existed {
                    std::fs::remove_file(&path)?;
                }
                done.push(json!({ "what": "git pre-commit hook removed", "path": path, "removed": existed }));
            }
            if *claude_code {
                let path = root.join(".claude").join("settings.json");
                let mut removed = 0;
                if path.exists() {
                    settings::edit_json_file(&path, |s| {
                        removed = settings::remove_hooks_matching(s, HOOK_MARKER);
                        Ok(())
                    })?;
                }
                done.push(json!({ "what": "Claude Code hooks removed", "path": path, "removed": removed }));
            }
            if *mcp {
                let path = root.join(".mcp.json");
                let mut removed = false;
                if path.exists() {
                    settings::edit_json_file(&path, |d| {
                        removed = settings::remove_mcp_server(d, "golab");
                        Ok(())
                    })?;
                }
                done.push(json!({ "what": "MCP server entry removed", "path": path, "removed": removed }));
            }
            if cli.json {
                emit(&json!({ "uninstalled": done }));
            } else {
                for d in &done {
                    println!("{} {}", st.green("✔"), d["what"].as_str().unwrap_or(""));
                }
            }
        }
        // The callbacks are dispatched before the store is opened, so these
        // arms are unreachable. Kept exhaustive rather than wildcarded so a
        // new callback cannot be added without wiring it.
        HookCmd::Guard { .. }
        | HookCmd::SessionStart
        | HookCmd::PostTool
        | HookCmd::SessionEnd => {
            bail!("hook callbacks are dispatched separately");
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn install_git_hook(root: &Path) -> Result<serde_json::Value> {
    let hooks = root.join(".git").join("hooks");
    if !hooks.exists() {
        bail!("no .git/hooks directory at {}", root.display());
    }
    let path = hooks.join("pre-commit");
    let exe = own_exe();
    let script = format!(
        "#!/bin/sh\n\
         # installed by `golab hook install`\n\
         # Refuses commits that touch symbols this agent has not leased.\n\
         # Set GOLAB_SKIP=1 to bypass for one commit.\n\
         [ -n \"$GOLAB_SKIP\" ] && exit 0\n\
         exec \"{exe}\" check\n"
    );
    std::fs::write(&path, script).with_context(|| format!("writing {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(json!({ "what": "git pre-commit hook installed", "path": path }))
}

fn install_claude_hooks(root: &Path) -> Result<serde_json::Value> {
    let path = root.join(".claude").join("settings.json");
    let exe = own_exe();
    let mut added = 0;
    settings::edit_json_file(&path, |s| {
        // Join the workspace and open the session, so the very first thing
        // the model sees is an oriented workspace it did not have to ask for.
        added += usize::from(settings::upsert_hook(
            s,
            "SessionStart",
            None,
            &format!("\"{exe}\" hook session-start"),
            15,
        ));
        // The one that blocks. Every editing tool, because an agent that can
        // reach around the guard through MultiEdit is not guarded.
        added += usize::from(settings::upsert_hook(
            s,
            "PreToolUse",
            Some("Edit|Write|MultiEdit|NotebookEdit"),
            &format!("\"{exe}\" hook guard"),
            10,
        ));
        // Where "publish progress" and "heartbeat" stop depending on the model
        // remembering to do either.
        added += usize::from(settings::upsert_hook(
            s,
            "PostToolUse",
            Some("Edit|Write|MultiEdit"),
            &format!("\"{exe}\" hook post-tool"),
            10,
        ));
        added += usize::from(settings::upsert_hook(
            s,
            "SessionEnd",
            None,
            &format!("\"{exe}\" hook session-end"),
            15,
        ));
        Ok(())
    })?;
    Ok(json!({ "what": "Claude Code hooks installed", "path": path, "added": added }))
}

fn install_mcp_server(root: &Path, as_agent: Option<&str>) -> Result<serde_json::Value> {
    let path = root.join(".mcp.json");
    let exe = own_exe();
    let mut args = vec![json!("mcp")];
    if let Some(name) = as_agent {
        args.push(json!("--as"));
        args.push(json!(name));
    }
    // No `--root`: clients launch MCP servers with the project as cwd, and
    // `find_workspace` walks up from there. Hardcoding an absolute root would
    // break the config for anyone else who checks the repo out elsewhere.
    let entry = json!({ "command": exe, "args": args });
    let mut changed = false;
    settings::edit_json_file(&path, |d| {
        changed = settings::upsert_mcp_server(d, "golab", entry.clone());
        Ok(())
    })?;
    Ok(json!({ "what": "MCP server registered", "path": path, "changed": changed }))
}

// ------------------------------------------------------------------- helpers

fn emit(value: &serde_json::Value) {
    println!(
        "{}",
        serde_json::to_string_pretty(value).unwrap_or_else(|_| "{}".to_string())
    );
}

fn first_line(s: &str) -> String {
    let line = s.lines().next().unwrap_or("");
    if line.chars().count() > 60 {
        let clipped: String = line.chars().take(57).collect();
        format!("{clipped}…")
    } else {
        line.to_string()
    }
}

/// `canonicalize` hands back `\\?\C:\...` on Windows, which is correct and
/// unreadable. Nothing we print needs the verbatim prefix.
fn strip_verbatim(p: PathBuf) -> PathBuf {
    match p.to_str().and_then(|s| s.strip_prefix(r"\\?\")) {
        Some(stripped) => PathBuf::from(stripped),
        None => p,
    }
}

fn workspace_root(cli: &Cli) -> Result<PathBuf> {
    if let Some(r) = &cli.root {
        return Ok(r.clone());
    }
    let cwd = std::env::current_dir()?;
    find_workspace(&cwd)
        .ok_or_else(|| anyhow!("no golab workspace here or in any parent — run `golab init` first"))
}

fn optional_agent(cli: &Cli, root: &Path) -> Option<String> {
    cli.agent
        .clone()
        .or_else(|| std::env::var("GOLAB_AGENT").ok().filter(|s| !s.is_empty()))
        .or_else(|| {
            std::fs::read_to_string(root.join(golab_core::RUNTIME_DIR).join("agent"))
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        })
}

fn require_agent(cli: &Cli, root: &Path) -> Result<String> {
    optional_agent(cli, root).ok_or_else(|| {
        anyhow!(
            "no agent identity — pass --agent, set GOLAB_AGENT, or run `golab agent register <name>`"
        )
    })
}
