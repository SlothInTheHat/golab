//! Core data types shared by the store, the CLI and the daemon.

use serde::{Deserialize, Serialize};

/// What kind of thing a symbol is. These are the leasable units of work:
/// the plan's "code is not just text" principle turned into rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SymbolKind {
    /// A deployable or publishable unit, discovered from a manifest
    /// (Cargo.toml, package.json, go.mod, pyproject.toml). Parent of its files.
    Service,
    File,
    Module,
    Class,
    Interface,
    Trait,
    Function,
    Method,
    Type,
    Constant,
    Macro,
    /// A database table, from DDL in a `.sql` file.
    Table,
}

impl SymbolKind {
    pub fn as_str(self) -> &'static str {
        match self {
            SymbolKind::Service => "service",
            SymbolKind::File => "file",
            SymbolKind::Module => "module",
            SymbolKind::Class => "class",
            SymbolKind::Interface => "interface",
            SymbolKind::Trait => "trait",
            SymbolKind::Function => "function",
            SymbolKind::Method => "method",
            SymbolKind::Type => "type",
            SymbolKind::Constant => "constant",
            SymbolKind::Macro => "macro",
            SymbolKind::Table => "table",
        }
    }

    pub fn parse(s: &str) -> Option<SymbolKind> {
        Some(match s {
            "service" => SymbolKind::Service,
            "file" => SymbolKind::File,
            "module" => SymbolKind::Module,
            "class" => SymbolKind::Class,
            "interface" => SymbolKind::Interface,
            "trait" => SymbolKind::Trait,
            "function" => SymbolKind::Function,
            "method" => SymbolKind::Method,
            "type" => SymbolKind::Type,
            "constant" => SymbolKind::Constant,
            "macro" => SymbolKind::Macro,
            "table" => SymbolKind::Table,
            _ => return None,
        })
    }

    /// Containers that exist to group other nodes rather than to be edited.
    pub fn is_container(self) -> bool {
        matches!(self, SymbolKind::Service | SymbolKind::File)
    }

    /// Kinds that hold executable bodies; used to decide what counts as a
    /// "real" edit target when reporting changes.
    pub fn is_callable(self) -> bool {
        matches!(self, SymbolKind::Function | SymbolKind::Method | SymbolKind::Macro)
    }
}

/// A node in the repository symbol graph.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Symbol {
    pub id: String,
    pub path: String,
    pub lang: String,
    pub kind: SymbolKind,
    /// Name as written in source, e.g. `processPayment`.
    pub name: String,
    /// Name qualified by enclosing scopes, e.g. `PaymentService.processPayment`.
    pub fqn: String,
    pub parent_id: Option<String>,
    pub start_byte: usize,
    pub end_byte: usize,
    pub start_line: usize,
    pub end_line: usize,
    /// Hash of the symbol's source text, nested symbols included.
    pub body_hash: String,
    /// Hash of the symbol's text excluding nested symbols; this is what
    /// "this symbol changed" actually means.
    pub own_hash: String,
    /// What this symbol is for, if the indexer could tell.
    pub role: Option<Role>,
    /// Role-specific detail: `{"method":"GET","path":"/payments/{id}"}` for an
    /// endpoint, `{"framework":"pytest"}` for a test, and so on.
    pub meta: serde_json::Value,
    /// Which registered repository this symbol belongs to. `"R1"` for every
    /// symbol until `golab repo add` registers a second one.
    pub repo_id: String,
}

impl Symbol {
    /// `src/pay.ts:PaymentService.processPayment` — the handle humans and
    /// agents actually type.
    pub fn handle(&self) -> String {
        match self.kind {
            SymbolKind::File => self.path.clone(),
            SymbolKind::Service => format!("service:{}", self.name),
            _ => format!("{}:{}", self.path, self.fqn),
        }
    }

    /// `GET /payments/{id}` for an endpoint, otherwise `None`.
    pub fn route(&self) -> Option<String> {
        if self.role != Some(Role::Api) {
            return None;
        }
        let method = self.meta.get("method")?.as_str()?;
        let path = self.meta.get("path")?.as_str()?;
        Some(format!("{method} {path}"))
    }

    pub fn location(&self) -> String {
        format!("{}:{}", self.path, self.start_line + 1)
    }
}

/// Directed edge in the repository knowledge graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EdgeKind {
    /// Caller invokes callee.
    Calls,
    /// Structural containment (service -> file -> class -> method).
    Contains,
    /// Type/interface reference.
    Uses,
    /// File imports file; service depends on service.
    Imports,
    /// Code reads or writes a database table.
    Queries,
    /// A test exercises a symbol.
    Tests,
}

impl EdgeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            EdgeKind::Calls => "calls",
            EdgeKind::Contains => "contains",
            EdgeKind::Uses => "uses",
            EdgeKind::Imports => "imports",
            EdgeKind::Queries => "queries",
            EdgeKind::Tests => "tests",
        }
    }
    pub fn parse(s: &str) -> Option<EdgeKind> {
        Some(match s {
            "calls" => EdgeKind::Calls,
            "contains" => EdgeKind::Contains,
            "uses" => EdgeKind::Uses,
            "imports" => EdgeKind::Imports,
            "queries" => EdgeKind::Queries,
            "tests" => EdgeKind::Tests,
            _ => return None,
        })
    }
    /// Edges that describe behaviour rather than structure — the ones that
    /// matter for "what breaks if I change this".
    pub fn is_semantic(self) -> bool {
        self != EdgeKind::Contains
    }
}

/// What a symbol *is for*, layered on top of what it structurally is. A
/// function is a function; being an HTTP handler or a test is a role.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// An HTTP route handler. `meta` carries method and path.
    Api,
    /// A test, or a file full of them.
    Test,
    /// Schema or migration.
    Schema,
    /// A deployable unit.
    Service,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Api => "api",
            Role::Test => "test",
            Role::Schema => "schema",
            Role::Service => "service",
        }
    }
    pub fn parse(s: &str) -> Option<Role> {
        Some(match s {
            "api" => Role::Api,
            "test" => Role::Test,
            "schema" => Role::Schema,
            "service" => Role::Service,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Edge {
    pub src: String,
    pub dst: String,
    pub kind: EdgeKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LeaseState {
    Active,
    Released,
    /// TTL ran out without a heartbeat — the crash-safety property.
    Expired,
    /// Taken away by a strictly higher-priority request.
    Preempted,
}

impl LeaseState {
    pub fn as_str(self) -> &'static str {
        match self {
            LeaseState::Active => "active",
            LeaseState::Released => "released",
            LeaseState::Expired => "expired",
            LeaseState::Preempted => "preempted",
        }
    }
    pub fn parse(s: &str) -> Option<LeaseState> {
        Some(match s {
            "active" => LeaseState::Active,
            "released" => LeaseState::Released,
            "expired" => LeaseState::Expired,
            "preempted" => LeaseState::Preempted,
            _ => return None,
        })
    }
}

/// A time-bounded claim on a symbol. Unlike a lock, it always expires.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Lease {
    pub id: String,
    pub symbol_id: String,
    pub symbol_handle: String,
    pub agent: String,
    pub task: Option<String>,
    pub priority: i64,
    pub ttl_secs: i64,
    pub acquired_at: i64,
    pub heartbeat_at: i64,
    pub expires_at: i64,
    pub state: LeaseState,
    pub note: Option<String>,
}

impl Lease {
    pub fn seconds_left(&self, now_ms: i64) -> i64 {
        (self.expires_at - now_ms).div_euclid(1000)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Agent {
    pub name: String,
    /// Free-form: `claude`, `cursor`, `human`, `ci`, ...
    pub kind: String,
    pub joined_at: i64,
    pub heartbeat_at: i64,
    pub status: String,
    /// Holds what it has, but the scheduler gives it nothing new.
    pub paused: bool,
    /// `None` = generalist. Set, `continue` only offers this agent tasks
    /// with no `required_capability` or a matching one.
    pub capability: Option<Capability>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskState {
    Pending,
    Running,
    Blocked,
    /// Submitted for approval. Leases are *not* released here — the work
    /// isn't merged yet — which is what distinguishes this from `Done`.
    Review,
    Done,
    Failed,
}

impl TaskState {
    pub fn as_str(self) -> &'static str {
        match self {
            TaskState::Pending => "pending",
            TaskState::Running => "running",
            TaskState::Blocked => "blocked",
            TaskState::Review => "review",
            TaskState::Done => "done",
            TaskState::Failed => "failed",
        }
    }
    pub fn parse(s: &str) -> Option<TaskState> {
        Some(match s {
            "pending" => TaskState::Pending,
            "running" => TaskState::Running,
            "blocked" => TaskState::Blocked,
            "review" => TaskState::Review,
            "done" => TaskState::Done,
            "failed" => TaskState::Failed,
            _ => return None,
        })
    }
}

/// A node in the task graph (Phase 2 seed).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    pub title: String,
    pub state: TaskState,
    pub priority: i64,
    pub assignee: Option<String>,
    pub deps: Vec<String>,
    pub created_at: i64,
    pub updated_at: i64,
    /// `None` = anyone can take it. Set, `continue` only offers it to a
    /// matching agent; `assign` bypasses this (human override).
    pub required_capability: Option<Capability>,
}

/// What a human actually asks for. Tasks are how the runtime breaks a goal
/// into leasable, schedulable pieces — but the goal is what a person named.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GoalState {
    Open,
    Done,
    Abandoned,
}

impl GoalState {
    pub fn as_str(self) -> &'static str {
        match self {
            GoalState::Open => "open",
            GoalState::Done => "done",
            GoalState::Abandoned => "abandoned",
        }
    }
    pub fn parse(s: &str) -> Option<GoalState> {
        Some(match s {
            "open" => GoalState::Open,
            "done" => GoalState::Done,
            "abandoned" => GoalState::Abandoned,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Goal {
    pub id: String,
    pub title: String,
    pub description: Option<String>,
    pub state: GoalState,
    pub priority: i64,
    pub created_at: i64,
    pub updated_at: i64,
    pub created_by: Option<String>,
}

/// How a goal's tasks are doing, in one glance.
#[derive(Debug, Clone, Serialize)]
pub struct GoalProgress {
    pub total: i64,
    pub done: i64,
    pub running: i64,
    pub in_review: i64,
    pub pending: i64,
    pub percent: f64,
    pub contributing_agents: Vec<String>,
}

/// A knowledge-graph-derived starting point for `goal decompose` — a
/// proposed cluster of symbols and a name for the task that would cover them.
/// Advisory only: it proposes *where* a goal's impact lands, not what to do
/// about it.
#[derive(Debug, Clone, Serialize)]
pub struct TaskSuggestion {
    pub title: String,
    pub symbols: Vec<String>,
}

/// A repository registered under this workspace. Every workspace has at
/// least `R1`, added automatically by `golab init`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Repo {
    pub id: String,
    pub name: String,
    /// Relative to the directory containing `.golab/`, so the workspace
    /// stays portable across machines and checkouts.
    pub root_path: String,
    pub added_at: i64,
}

/// A role an agent can specialize in, and a task can require. Models come
/// and go; roles are the stable vocabulary a scheduler matches against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Capability {
    Planner,
    Backend,
    Frontend,
    Database,
    Testing,
    Security,
    Documentation,
    Reviewer,
}

impl Capability {
    pub fn as_str(self) -> &'static str {
        match self {
            Capability::Planner => "planner",
            Capability::Backend => "backend",
            Capability::Frontend => "frontend",
            Capability::Database => "database",
            Capability::Testing => "testing",
            Capability::Security => "security",
            Capability::Documentation => "documentation",
            Capability::Reviewer => "reviewer",
        }
    }
    pub fn parse(s: &str) -> Option<Capability> {
        Some(match s {
            "planner" => Capability::Planner,
            "backend" => Capability::Backend,
            "frontend" => Capability::Frontend,
            "database" => Capability::Database,
            "testing" => Capability::Testing,
            "security" => Capability::Security,
            "documentation" => Capability::Documentation,
            "reviewer" => Capability::Reviewer,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub id: i64,
    pub ts: i64,
    pub kind: String,
    pub agent: Option<String>,
    pub symbol_handle: Option<String>,
    pub task: Option<String>,
    pub detail: serde_json::Value,
}

/// Shared memory entry: architecture decisions, conventions, interfaces.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryEntry {
    pub key: String,
    pub value: String,
    pub author: Option<String>,
    pub tags: Vec<String>,
    pub updated_at: i64,
}

/// Structured agent-to-agent message (Phase 3 seed).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub id: i64,
    pub from: String,
    pub to: String,
    pub subject: String,
    pub body: serde_json::Value,
    pub sent_at: i64,
    pub read_at: Option<i64>,
    pub reply_to: Option<i64>,
}

/// Well-known request kinds. The field is a plain string so a team can invent
/// its own protocol, but these have behaviour attached.
pub mod request_kind {
    /// "Hand me that lease." Accepting performs the transfer atomically.
    pub const LEASE_TRANSFER: &str = "lease-transfer";
    /// "I need this interface to exist before I can continue."
    pub const INTERFACE: &str = "interface";
    /// "I am blocked until that task is done." Auto-fulfils on completion.
    pub const DEPENDENCY: &str = "dependency";
    /// "Please look at this."
    pub const REVIEW: &str = "review";
    /// "This changed and your active work may depend on it." Opened by the
    /// runtime itself, not another agent — informational, not a question.
    pub const API_CHANGE: &str = "api-change";
    /// Anything else.
    pub const QUESTION: &str = "question";
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RequestState {
    /// Awaiting an answer.
    Open,
    /// The recipient committed to it but has not delivered yet.
    Accepted,
    /// Answered no.
    Declined,
    /// Delivered. This is what a waiting agent is looking for.
    Fulfilled,
    /// Withdrawn by the requester.
    Cancelled,
    /// The deadline passed with no answer.
    Expired,
}

impl RequestState {
    pub fn as_str(self) -> &'static str {
        match self {
            RequestState::Open => "open",
            RequestState::Accepted => "accepted",
            RequestState::Declined => "declined",
            RequestState::Fulfilled => "fulfilled",
            RequestState::Cancelled => "cancelled",
            RequestState::Expired => "expired",
        }
    }
    pub fn parse(s: &str) -> Option<RequestState> {
        Some(match s {
            "open" => RequestState::Open,
            "accepted" => RequestState::Accepted,
            "declined" => RequestState::Declined,
            "fulfilled" => RequestState::Fulfilled,
            "cancelled" => RequestState::Cancelled,
            "expired" => RequestState::Expired,
            _ => return None,
        })
    }
    /// Still worth polling.
    pub fn is_live(self) -> bool {
        matches!(self, RequestState::Open | RequestState::Accepted)
    }
}

/// A structured ask from one agent to another: the unit of negotiation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub id: String,
    pub kind: String,
    pub from: String,
    /// `None` means broadcast: whoever can answer, may.
    pub to: Option<String>,
    pub subject: String,
    pub body: serde_json::Value,
    /// The symbol being negotiated over, for `lease-transfer` and friends.
    pub resource_symbol: Option<String>,
    pub resource_handle: Option<String>,
    /// The task being waited on, for `dependency`.
    pub resource_task: Option<String>,
    /// The requester's own task context.
    pub task: Option<String>,
    pub priority: i64,
    pub state: RequestState,
    pub created_at: i64,
    pub deadline_at: Option<i64>,
    pub resolved_at: Option<i64>,
    pub resolver: Option<String>,
    pub response: Option<serde_json::Value>,
}

impl Request {
    pub fn seconds_until_deadline(&self, now_ms: i64) -> Option<i64> {
        self.deadline_at.map(|d| (d - now_ms).div_euclid(1000))
    }
}

/// "Where I am, so nobody has to ask." Progress is advisory, not a promise.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProgressUpdate {
    pub id: i64,
    pub agent: String,
    pub task: Option<String>,
    pub symbol_id: Option<String>,
    pub symbol_handle: Option<String>,
    pub percent: Option<i64>,
    pub eta_secs: Option<i64>,
    pub note: Option<String>,
    pub ts: i64,
}

/// Why an acquire request could not be granted.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Conflict {
    /// The symbol whose lease blocks us — may be an ancestor or descendant
    /// of the requested symbol, not the symbol itself.
    pub blocking_symbol: String,
    pub relation: ConflictRelation,
    pub holder: String,
    pub task: Option<String>,
    pub priority: i64,
    pub expires_at: i64,
    pub seconds_until_free: i64,
    pub lease_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ConflictRelation {
    /// Same symbol.
    Same,
    /// Someone holds a scope that contains the requested symbol.
    Ancestor,
    /// Someone holds something inside the requested scope.
    Descendant,
    /// Nobody holds it, but another agent is ahead in the wait queue.
    Queued,
}

impl ConflictRelation {
    pub fn as_str(self) -> &'static str {
        match self {
            ConflictRelation::Same => "same",
            ConflictRelation::Ancestor => "ancestor",
            ConflictRelation::Descendant => "descendant",
            ConflictRelation::Queued => "queued",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum AcquireOutcome {
    /// Fresh lease.
    Granted { lease: Lease },
    /// Caller already held it; TTL extended instead.
    Extended { lease: Lease },
    /// Blocked. `conflicts` is non-empty and tells the agent what to wait for.
    Denied { conflicts: Vec<Conflict> },
}

impl AcquireOutcome {
    pub fn lease(&self) -> Option<&Lease> {
        match self {
            AcquireOutcome::Granted { lease } | AcquireOutcome::Extended { lease } => Some(lease),
            AcquireOutcome::Denied { .. } => None,
        }
    }
    pub fn is_granted(&self) -> bool {
        self.lease().is_some()
    }
}

/// How a symbol differs between the indexed snapshot and the working tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChangeKind {
    Added,
    Modified,
    Removed,
}

impl ChangeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ChangeKind::Added => "added",
            ChangeKind::Modified => "modified",
            ChangeKind::Removed => "removed",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SymbolChange {
    pub handle: String,
    pub path: String,
    pub fqn: String,
    pub kind: SymbolKind,
    pub change: ChangeKind,
    /// The symbol id, when it exists in the index (always for modified/removed).
    pub symbol_id: Option<String>,
    /// Nearest indexed ancestor — what an agent must hold to legally add a
    /// brand new symbol inside an existing scope.
    pub covering_id: Option<String>,
}

/// Result of enforcing "you may only edit what you lease".
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckReport {
    pub agent: String,
    pub changes: Vec<SymbolChange>,
    pub violations: Vec<Violation>,
    pub covered: Vec<CoveredChange>,
    pub unparsed: Vec<String>,
}

impl CheckReport {
    pub fn ok(&self) -> bool {
        self.violations.is_empty()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Violation {
    pub change: SymbolChange,
    /// Empty when nobody holds it (the agent simply never leased it).
    pub held_by: Option<Conflict>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoveredChange {
    pub change: SymbolChange,
    /// `None` for changes nothing could own yet, such as a brand new file.
    pub lease_id: Option<String>,
    /// Which symbol the satisfying lease is on (self or an ancestor).
    pub via: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `parse()` has a catch-all `_ => None`, so a forgotten arm compiles
    /// fine and only fails at runtime (e.g. the daemon rejecting a state
    /// string nothing told it about). Every variant round-tripping through
    /// its own `as_str()` is cheap insurance against that, for this and any
    /// future variant.
    #[test]
    fn task_state_strings_round_trip() {
        for s in [
            TaskState::Pending,
            TaskState::Running,
            TaskState::Blocked,
            TaskState::Review,
            TaskState::Done,
            TaskState::Failed,
        ] {
            assert_eq!(TaskState::parse(s.as_str()), Some(s), "{s:?}");
        }
    }

    #[test]
    fn goal_state_strings_round_trip() {
        for s in [GoalState::Open, GoalState::Done, GoalState::Abandoned] {
            assert_eq!(GoalState::parse(s.as_str()), Some(s), "{s:?}");
        }
    }

    #[test]
    fn capability_strings_round_trip() {
        for c in [
            Capability::Planner,
            Capability::Backend,
            Capability::Frontend,
            Capability::Database,
            Capability::Testing,
            Capability::Security,
            Capability::Documentation,
            Capability::Reviewer,
        ] {
            assert_eq!(Capability::parse(c.as_str()), Some(c), "{c:?}");
        }
    }
}
