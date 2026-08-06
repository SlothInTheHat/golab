# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

`golab` is an operating system for collaborative AI software engineering: a
human states a goal, and the runtime coordinates every human and agent
working toward it. The user-facing vocabulary is `goal`, `swarm`, `continue`,
`assign`, `observe`, `review` — a human thinks in goals, not leases. Underneath
that sits the actual coordination machinery: a tree-sitter knowledge graph of
the repository, time-bounded **leases** on symbols, enforcement that refuses
unleased edits, a wave-based scheduler, an agent-to-agent negotiation
protocol, and an event bus. None of that is deprecated or hidden — it is real
infrastructure that the everyday commands are built on, and reaching for it
directly (`lease acquire`, `task next`, `schedule`) still works exactly as
documented. `plan.md` is the original 11-phase vision document; `README.md`
documents what is actually built, framed around the goal-first vocabulary.
Phases 0-5 are complete, plus the adapter layer below; Phase 10 is partially
started (`goal suggest`); 6-9 are untouched.

### The adapter layer

golab does not replace coding agents — it coordinates them. Claude Code,
Cursor, Codex, Windsurf, Zed and Gemini CLI all speak MCP, so **one** stdio
server (`golab mcp`) reaches all of them and per-tool work collapses to a
config snippet. Two properties make it an adapter rather than a prompt:

- **Lifecycle never depends on the model.** Registering, heartbeating, renewing
  leases, receiving notices and leaving cleanly all happen in the `initialize`
  handler and a background thread. A model that calls no tools still
  participates correctly.
- **A refusal is not an error.** The exit-code doctrine (`0` yes, `1` a
  legitimate no, `2` broken) maps onto MCP as: denials come back with
  `isError: false` and the "no" in `structuredContent`, so a model branches on
  it and goes to negotiate rather than treating it as a fault.

`golab hook install --claude-code --mcp` wires an editor up: `PreToolUse`
*blocks* an edit to something another agent holds, `SessionStart`/`SessionEnd`
join and leave, `PostToolUse` publishes progress. Hooks fire whether the model
cooperates or not, which is what makes enforcement real rather than advisory.

### Layering convention for new work

New user-facing capability goes through the goal/swarm/assign/observe/review/
continue vocabulary, not bolted directly onto `lease`/`schedule` primitives.
Concretely: add the `Store` method the capability needs, wire it into
whichever of the six top-level commands it conceptually belongs to (or add a
seventh only if none of them fit), and give the CLI and the dashboard the
same access to it — the existing rule for `Store` methods generally, per
"The dashboard is a thin client" below. The primitives underneath stay stable
and keep their own tests; the six verbs are thin orchestration over them, not
a parallel implementation.

## Commands

```bash
cargo build                       # debug binary at target/debug/golab
cargo build --release
cargo test                        # 364 tests (~40s)
cargo clippy --all-targets        # kept at zero warnings
```

Running a subset — the filter is a substring of the full test path:

```bash
cargo test -p golab-core lease::tests::expired_leases_free_themselves
cargo test -p golab-core imports::          # one module's unit tests
cargo test -p golab-cli --test knowledge    # one integration suite
```

Demos (throwaway temp workspaces; safe to run repeatedly):

```bash
bash demo/goals.sh         # the everyday vocabulary  (also demo/goals.ps1)
bash demo/demo.sh          # the primitives underneath (also demo/demo.ps1)
bash demo/negotiate.sh     # two autonomous agents negotiating (also .ps1)
bash demo/mcp.sh           # two coding tools sharing a repo over MCP (also .ps1)
bash demo/knowledge.sh     # the knowledge graph + live re-indexing
bash demo/schedule.sh      # the scheduler: inference, waves, reassignment
bash demo/dashboard.sh     # seeds a busy workspace and serves the dashboard
bash demo/workspace.sh     # the live workspace: two tools, a refused edit, a
                           # notification and the repository picture (also .ps1)
```

Dogfooding is the fastest smoke test — golab indexes itself:

```bash
./target/debug/golab.exe init && ./target/debug/golab.exe index
./target/debug/golab.exe api        # finds the daemon's own axum routes
./target/debug/golab.exe services   # finds its own four crates
rm -rf .golab                       # .golab/ is gitignored; delete when done
```

## Architecture

Four crates: `golab-core` (all logic), `golab-daemon` (HTTP/WebSocket/dashboard
+ filesystem watcher), `golab-mcp` (the MCP adapter, stdio), `golab-cli` (the
`golab` binary; depends on all three).

The rule is not "three crates" — it is *core holds all the logic; a transport
crate wraps it; the CLI is the binary that depends on the transports*.
`golab-mcp` is a second transport over the same `Store`, with a completely
different dependency profile (no axum, no tokio, no tower-http) and a
synchronous three-thread design, so it is a peer of `golab-daemon` rather than
a module inside it.

### The central invariant: coordination lives in SQLite, not in a server

Every mutation runs inside `Store::write`, which opens an `IMMEDIATE`
transaction. Two agents in two OS processes cannot both win the same symbol,
and **nothing has to be running** for `golab lease acquire` to be correct. The
daemon adds observability, not safety. `crates/golab-cli/tests/cli.rs` proves
this by racing eight real processes and asserting exactly one exits 0 — if you
change the lease path, that test is the one that matters.

### `impl Store` is spread across modules

`Store` is defined in `store.rs` but its methods live wherever they belong:
`lease.rs`, `protocol.rs`, `work.rs`, `graph.rs`. Searching `store.rs` alone
will not find a method. This is deliberate — one connection type, cohesive
modules.

### Data flow

```
scan.rs ──> parse.rs (tree-sitter) ──> symbols
        ──> imports.rs   ──> file→file edges, and the scope for call resolution
        ──> roles.rs     ──> api / test roles + metadata
        ──> sql.rs       ──> table nodes + queries edges
        ──> topology.rs  ──> services from manifests, CODEOWNERS ownership

arch.rs   collapses all of the above into the picture a *human* holds:
          services, their directories, the tables they query, and who is
          inside each box right now. Derived, never a second source of truth.
activity.rs  who has their hands on which file *this second* — fed by the
          editor hooks and the MCP tools, expired by `sweep()`.
notice.rs a query over `requests` + `events`, phrased for a person. The
          half of `notify.rs` that previously only ever reached an agent.
```

`scan()` is **two-pass on purpose**: every node in the repository is written
first, then edges are resolved. Imports resolve before calls, because knowing
`pay.ts` imports `./ledger` is what lets `record()` resolve to the right one of
several same-named functions. Resolution order is: same file → imported files →
repo-wide unique name → unresolved (never guessed).

### Identity vs. content

- `ids::symbol_id` hashes **repo_id + path + kind + fqn + disambiguator** —
  never content. This is why a lease survives an edit to the function it
  covers, and why a rename correctly retires it.
- `body_hash` = the symbol's whole text. `own_hash` = its text with nested
  symbols elided. Enforcement compares `own_hash`, so editing a method does not
  count as editing its class. Changing this breaks `check.rs`.
- The domain prefix hashed into every symbol id (`golab.symbol.v2\0` today)
  should bump whenever the fields being hashed change, even though there are
  no schema migrations to protect — a `repo_id`/schema change already forces
  a fresh database either way. What the bump actually guards against is
  binary skew: a daemon on the old binary and a CLI on the new one hitting
  the same live `runtime.db` mid-deploy would otherwise compute colliding ids
  under two different formulas.

### Containment

`service → file → class → method`, via `symbols.parent_id`. Leases are
containment-aware in both directions: holding a class blocks its methods, and
holding a method blocks the class. Services parent their files, so one lease can
cover a whole crate. `Store::ancestors` / `descendants` walk this; `check.rs`
uses `ancestors` to decide whether a lease covers a change.

### The scheduler reuses the lease rules rather than reimplementing them

`schedule.rs` decides whether a task is safe to hand out by calling
`lease::conflicts_tx` — the same function `acquire_tx` uses — and claims by
calling `lease::acquire_in_tx` inside the *same* transaction as the task
update. If those two ever diverge, the scheduler will promise work that the
lease layer then refuses. A task's scope and its leases are granted together
and returned together: `release_task_leases` is the inverse of `claim_in_tx`,
and both completion and reassignment call it.

### Lazy expiry

Leases, requests and tasks orphaned by dead agents are reclaimed when someone
next writes, watches, or the daemon ticks — via `Store::sweep()`, which is
`expire_due()` + `expire_requests()` + `expire_sessions()` +
`expire_activity()` + `reassign_orphans()`.
Acquisition is never stale (it sweeps first), but a bare `lease list` can read a
beat behind. Add new expiry sweeps to `sweep()`, not to individual commands.

### The dashboard is a thin client, not a second implementation

Every button in `dashboard.html` posts to an endpoint that calls the same
`Store` method the CLI calls — `set_agent_paused`, `reassign_task`,
`set_task_priority`. Throughput is a query over the `events` table rather than
a separate set of counters, so it cannot drift from what actually happened.
When adding a capability, add the `Store` method first and give the CLI and the
dashboard the same access to it.

The repository picture is the same rule one level up: the SVG in
`dashboard.html` lays out and paints what `GET /api/arch` returns, and works
out nothing about the repository itself. Which files belong to which service,
which service depends on which, and who is inside each box are all decided in
`arch.rs`, so `golab arch` in a terminal and the graph in a browser cannot
disagree.

**The browser does not poll.** It opens `/ws`, receives one coalesced snapshot
per tick, and repaints. The only `setInterval` left is the lease countdown,
which needs no server. Adding a `setInterval(fetch…)` to this file is
re-introducing the bug described under "Events are the only narrative" below —
put the field on the snapshot instead.

### Events are the only narrative

Everything interesting goes through `store::emit`. `golab watch`, the dashboard
and audits all read the same `events` table. The daemon **polls the table**
rather than broadcasting in-process, because CLI invocations in other processes
write events too and must show up identically. The MCP adapter's heartbeat
thread polls it for exactly the same reason.

`pump()` also builds **one state snapshot per tick** and broadcasts it on
`/ws`, so the frames are tagged: `{"type":"event",…}` and
`{"type":"snapshot",…}`. Coalescing has to happen here rather than in the
client — the dashboard used to refetch `/api/status` from `ws.onmessage`, once
per event, which turned a burst of two hundred events into two hundred requests
from every open browser exactly when the server was busiest. There is a
`SNAPSHOT_FLOOR_MS` heartbeat too, because a lease ticking toward expiry
changes the picture without writing anything.

### `guard` is the predictive twin of `check`

`check.rs` is post-hoc — it diffs the working tree and reports edits already
made, which is the right shape for a pre-commit hook. `guard.rs` answers the
question one keystroke earlier ("may I touch this at all?") so an agent can
negotiate instead of being told off at commit time. It re-derives nothing:
`Store::conflicts_for` already walks *both* directions of containment, so
calling it on a file symbol covers that file's whole subtree and its enclosing
service in one query. Keep the two consistent — a guard that refused what
`acquire` would grant, or vice versa, would be worse than no guard at all
(there is a test pinning this).

### Notices ride on tool results, not on notifications

MCP gives a server no way to inject a turn, and clients almost never surface
`notifications/message` to the model. So `golab-mcp` attaches a `notices` block
to **every** tool result, in both the structured and text halves. That is the
only channel with a guaranteed path into a model's context; the
`resources/list_changed` notification is best-effort and nothing may depend on
it. Don't "fix" this by moving it to server-initiated notifications.

## Conventions

- **Exit codes**: `0` success, `1` a legitimate "no" (lease denied, check
  violations, no ready task, no test coverage), `2` an error. Agents branch on
  this, so a denial must never be an error.
- **`--json` on every command.** It is the agent-facing interface; keep parity
  when adding output.
- **Symbol references** are resolved by `Store::resolve` in precedence order:
  id → `service:name` → `path:Fqn` → file path → fqn → bare name → substring.
  Ambiguity returns an error listing candidates rather than picking one.

## Gotchas that cost real debugging time

- **The schema has no migrations.** `store.rs` runs `CREATE TABLE IF NOT
  EXISTS`, so adding a column does nothing to an existing database. After a
  schema change, delete `.golab/runtime.db` (tests use temp dirs, so they pass
  regardless — this bites only manual testing).
- **New tables, never new columns.** The corollary of the above, and the rule
  to reach for first: a new `CREATE TABLE IF NOT EXISTS` self-heals on an
  existing database where `ALTER TABLE ADD COLUMN` is a silent no-op. Both
  `task_goals` and `sessions` exist as tables for this reason, and both are
  better designs anyway — one agent legitimately has several sessions, and a
  stale `agents` row versus a live session is exactly the distinction the
  dashboard needs. If you genuinely cannot avoid a column, bump
  `store::SCHEMA_VERSION`; `Store::open` then **errors** (never warns) on a
  mismatch. A warning would be useless here — the processes most likely to hit
  it, the daemon and an MCP adapter, log to stderr nobody reads.
- **In `golab mcp`, stdout is a protocol channel.** A stray `println!`
  anywhere reachable from it corrupts a live session in a way that is
  miserable to diagnose, which is why `golab-mcp` carries
  `#![deny(clippy::print_stdout, clippy::print_stderr)]` and why `golab-core`
  contains no printing at all — all of golab's output lives in `golab-cli`.
  The CLI's `Mcp` arm also handles its own errors rather than propagating,
  because `main` reports failures as JSON *on stdout* under `--json`.
- **Adding a `symbols` column means four edits**: the `SCHEMA` string,
  `SYMBOL_COLS`, `QUALIFIED_COLS` (joined queries offset their extra columns by
  the column count — see `graph.rs` reading edge kind at index 16), and
  `row_to_symbol`, plus the upserts in `scan.rs`. Append the column rather than
  inserting it earlier in the list — inserting shifts every positional index
  after it, silently.
- **`path` is not a global key — `repo_id` is part of it.** A workspace can
  register more than one repository (`golab repo add`), and two repos can
  share an identical relative path. Every `path`-keyed read or write in
  `store.rs`/`scan.rs` (file hashing, the full-scan prune diff, edge
  resolution) must filter by `repo_id` too, or scanning one repo can delete
  another repo's symbols and leases as collateral damage of "prune what's no
  longer on disk." `ids::symbol_id` hashes `repo_id` first for the same
  reason — identity, not just storage, has to account for it.
- **`activity` is not a lease and not a progress row.** All three describe an
  agent's relationship to code and they answer different questions: a lease is
  what you *own* (and outlives the edit), progress is a point in time (appended
  forever), activity is where your hands are *this second* (upserted per
  `(agent, repo, path)`, expired by `sweep`). Reaching for the wrong one gives
  an answer that is stale by a whole task.
- **Activity is only as good as the tools reporting it.** golab does not watch
  keystrokes or read unsaved buffers: `hooks::guard` records it before an edit,
  `hooks::post_tool` and the MCP tools after. A tool wired up with neither
  contributes presence but no activity, and that is a documented limit rather
  than a bug to fix by inventing data.
- **Every activity write is best-effort.** `hooks::guard` is on the critical
  path of a keystroke; a failed insert must never change a verdict, an exit
  code, or how long an editor waits. `.ok()` is deliberate at every call site.
- **TypeScript's `TAGS_QUERY` is supplementary to JavaScript's.** Loading it
  alone matches nothing. `lang.rs` layers `[JS_TAGS, TS_TAGS]`; any new grammar
  needs checking for the same pattern.
- **tree-sitter's Rust API does not evaluate query predicates.** `#not-eq?`,
  `#match?` and friends parse but never filter; filter in Rust after the match.
- **Ordering by wall-clock milliseconds is a flake.** The lease queue uses a
  monotonic `seq` because two agents genuinely queue inside the same
  millisecond.
- **Preemption is not a release.** It swaps one holder for another, so it must
  not auto-resolve pending lease-transfer requests the way release and expiry
  do (`lease.rs::finish_lease`).
- **Role detection is regex over source, not per-framework plugins.** Routes in
  comments are filtered; routes inside string fixtures in tests are not, and are
  indexed as real. Prefer widening the cross-language heuristic over adding a
  framework special case.

## Testing layout

Unit tests are inline (`#[cfg(test)] mod tests`) in each `golab-core` and
`golab-mcp` module, and in `golab-daemon/src/watcher.rs`. Integration tests
drive the **real binary** via `env!("CARGO_BIN_EXE_golab")` in
`crates/golab-cli/tests/`: `cli.rs` (leasing and enforcement, including the
multi-process race), `negotiation.rs` (Phase 3), `knowledge.rs` (Phase 4),
`guard.rs` (the predictive guard and context packets), `hooks.rs` (editor hook
install and callbacks), `mcp.rs` (a real MCP client speaking to a spawned
server). Tests that need concurrency spawn actual processes rather than
threads, because cross-process correctness is the claim being tested.

`live.rs` carries a hand-rolled websocket client for the same reason `mcp.rs`
hand-rolls its JSON-RPC harness: the surface needed is "connect and read text
frames", and a dependency for one test file is a worse trade. Its two
load-bearing cases are `the_socket_pushes_a_whole_picture_before_anything_else`
(a page that cannot paint from one frame has to fetch, and the polling would
grow straight back) and `a_burst_of_events_is_one_snapshot_not_one_per_event`.
Its `drain` helper deliberately waits *through* quiet stretches — silence is
what some of those tests measure.

`mcp.rs` carries a small client harness with a reader thread and
`recv_timeout` — without it a server bug hangs CI instead of failing it. Two of
its cases are load-bearing and worth keeping: `stdout_carries_only_protocol_
frames` (the one failure mode that silently bricks every user) and
`heartbeat_keeps_the_agent_online_with_no_tool_calls` (proof that lifecycle
does not depend on the model).
