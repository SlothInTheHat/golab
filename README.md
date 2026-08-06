# atlas — an operating system for collaborative AI software engineering

**Git stores history. atlas schedules the work.**

When several humans and several AI coding agents — Claude Code, Cursor,
OpenCode, a CI bot, a docs bot — work on one repository at the same time, the
bottleneck stops being code generation and becomes coordination: nobody knows
who is editing what, which interfaces are mid-change, what is blocked, or
what another agent already figured out.

atlas is a runtime that sits between a human's goal and the repository, and
coordinates every human and agent working toward it:

```
Human Goal → Planning → Task Graph → Scheduler → Shared Memory
    → Knowledge Graph → Leases → Execution → Verification
```

A human states a goal. atlas breaks it into a task graph, hands tasks to
whoever's available without letting two of them collide, and keeps a
verification checkpoint before anything counts as done. Everything at the
bottom of that stack — wave-based scheduling, a tree-sitter knowledge graph,
symbol-level leases, an agent-to-agent negotiation protocol — is real,
load-bearing infrastructure. It is also not what you should have to think
about. You think about goals; the runtime decides how to coordinate
everything underneath.

```
$ atlas goal add "Implement refunds" --priority 9
✔ G1 Implement refunds

$ atlas goal decompose G1 --task "wire the endpoint" --symbol createPayment
✔ T1 wire the endpoint (under G1)

$ atlas --agent claude-1 continue --goal G1
▶ T1    wire the endpoint    running  p9 → claude-1  ⟨api/src/routes.ts:createPayment⟩

$ atlas observe
who's doing what
  ● claude-1         claude     1 lease(s), task=T1, goal=G1
0 startable now · up to 0 in parallel · 0 in review
```

That `continue` claimed the highest-priority task claude-1 could safely
start, and leased the exact symbols it scopes — the same lease `atlas lease
acquire` grants directly, for when you want the primitive itself. See
[Foundations](#foundations--how-the-runtime-actually-works) below for how
that's built.

---

## Quick start

```bash
cargo build --release                     # produces target/release/atlas

cd your-repo
atlas init                                # creates .atlas/runtime.db
atlas index                               # build the knowledge graph

atlas goal add "Implement refunds" --priority 9
atlas goal decompose G1 --task "wire the endpoint" --symbol createPayment

atlas --agent claude-1 swarm join claude-1
atlas --agent claude-1 continue --goal G1    # claims a task, leases its scope
# ... make the edit ...
atlas check --agent claude-1                 # only what you hold was touched
atlas --agent claude-1 review submit T1      # done, awaiting approval; leases held
atlas --agent reviewer-1 review approve T1   # releases leases, unblocks dependents
```

Want the primitive directly instead of going through a task? It still works,
unchanged:

```bash
atlas lease acquire processPayment --ttl 300 --task stripe
atlas check
atlas lease release --all
```

See the whole thing end to end:

```bash
bash demo/goals.sh          # the everyday vocabulary   (also demo/goals.ps1)
bash demo/demo.sh           # the primitives underneath (also demo/demo.ps1)
```

Watch two autonomous agents resolve a conflict between themselves, with no
human input at any point:

```bash
bash demo/negotiate.sh     # or:  powershell -File demo/negotiate.ps1
```

Or the same conflict resolved between two *real coding tools* over MCP, with
neither one running a lease command:

```bash
bash demo/mcp.sh           # or:  powershell -File demo/mcp.ps1
```

Tour the repository knowledge graph — services, routes, tables, coverage,
ownership — and watch it update itself as a file changes:

```bash
bash demo/knowledge.sh
```

Watch the scheduler order work from the call graph, hand it to three agents
without collisions, and take it back when one dies:

```bash
bash demo/schedule.sh
```

See the dashboard with something actually happening on it — this seeds a
workspace with four agents, a backlog, live progress and an open negotiation,
then serves it:

```bash
bash demo/dashboard.sh     # http://127.0.0.1:7373
```

Or watch the whole thing happen — two developers on two different coding
tools, a live edit visible before it is committed, and an API change that
notifies whoever depends on it, with no commits and no pull requests:

```bash
bash demo/workspace.sh     # or:  powershell -File demo/workspace.ps1
```

Or point it at your own repo:

```bash
atlas serve                # http://127.0.0.1:7373
```

---

## Connect your coding tool

Everything above works by hand. It should not have to be done by hand.

atlas does not replace Claude Code, Cursor or Codex — it coordinates them. All
of them speak **MCP**, so one stdio server (`atlas mcp`) covers every one, and
connecting a tool is a config snippet rather than an integration project. Once
connected, a developer keeps their editor, never types a lease command, and
still participates in a coordinated multi-agent workspace.

```bash
atlas hook install --mcp            # writes .mcp.json
atlas hook install --claude-code    # writes .claude/settings.json hooks
```

Both merge into whatever is already in those files and are safe to run twice.
`atlas hook uninstall --mcp --claude-code` takes only atlas's own entries back
out. For other tools, the entry is the same shape:

```jsonc
// Cursor: .cursor/mcp.json   ·   Codex/Windsurf/Zed/Gemini: their own config
{ "mcpServers": { "atlas": { "command": "/abs/path/to/atlas", "args": ["mcp"] } } }
```

See it with two tools in one repository:

```bash
bash demo/mcp.sh           # or:  powershell -File demo/mcp.ps1
```

### What the adapter does on its own

The point of an adapter rather than a prompt is that none of this depends on
the model choosing to cooperate:

| | |
|---|---|
| **registers** with the workspace | on the MCP handshake |
| **heartbeats** and **renews leases** | a background thread, every 20s |
| **receives notices** from other agents | attached to every tool result |
| **leaves cleanly**, handing work back | when the editor closes |

A model that calls no atlas tools at all still joins the swarm, holds its
leases, and releases them when you quit. What it *gains* by calling tools is
work (`next_task`), orientation (`task_context`), and the ability to negotiate
(`ask`, `respond`).

### Enforcement, at the moment of the edit

With the Claude Code hooks installed, an edit to something another agent holds
is **refused** — not warned about at commit time:

```
atlas: src/pay.ts:charge is held by alice for task T4 for another 3m 20s.
Ask for it with the atlas `ask` tool (kind=lease-transfer,
symbol="src/pay.ts:charge"). If your change does not touch charge, you may
edit a different symbol in this file instead.
```

That message goes to the model, so it can go and negotiate by itself. Accepting
a `lease-transfer` performs the handover atomically, and the work continues —
no human brokered anything.

An unleased but *uncontended* edit is allowed. atlas blocks collisions, not
work.

### The same thing without MCP

`atlas guard <path>` answers the question directly — exit `0` if you may edit
it, `1` if somebody else holds it — so any tool that can run a command can
enforce the same rule:

```bash
atlas guard src/pay.ts --json          # verdict, who holds it, what to do next
atlas context --task T1                # scope, blast radius, tests, decisions
atlas session list                     # which tools are attached right now
atlas activity                         # who is editing what, right now
atlas arch                             # services, dependencies, who is inside each
```

---

## Concepts

These six verbs are the whole everyday surface. Nothing below this section is
something you should need day to day — it exists so these six work correctly.

### Goal

A goal is the thing a human actually wants, stated once:

```bash
atlas goal add "Implement refunds" --priority 9 --description "..."
atlas goal list
atlas goal show G1              # progress, contributors, tasks under it
atlas goal done G1 / abandon G1
```

### Decomposing a goal into a task graph

A goal isn't executable; tasks are. Decomposition has two halves, and both
are supported because neither alone is enough:

**Explicit** — you (or an agent) know exactly what needs to happen and to
which symbols:

```bash
atlas goal decompose G1 --task "wire the endpoint" --symbol createPayment --priority 9
```

**Graph-assisted** — you know *where* the change starts but not yet how to
carve it up. `suggest` walks the knowledge graph's impact radius from a
symbol and clusters what it finds by owning service, proposing one task per
cluster:

```bash
$ atlas goal suggest G1 --near voidPayment --depth 2
advisory: where the impact lands, not what to do about it
  Update payments-api (1 symbol(s))
    api/src/routes.ts:voidPayment
    atlas goal decompose G1 --task "Update payments-api" --symbol api/src/routes.ts:voidPayment
```

That's deliberately advisory, not generated intent — the graph can tell you
*where* a change touches, not *what* to do about it. `--apply` turns the
preview directly into tasks instead of just printing the commands.

**Fully automatic** — `goal plan` is `suggest` with no `--near` required: it
keyword-matches the goal's own title and description against the graph to
find candidate entry points, then clusters exactly the same way:

```
$ atlas goal add "Implement refunds"
$ atlas goal plan G1
advisory: where the impact lands, not what to do about it
  Update payments-api (1 symbol(s))
    api/src/routes.ts:refund
    atlas goal decompose G1 --task "Update payments-api" --symbol api/src/routes.ts:refund
```

There's still no model in the loop — a title with nothing resembling it in
the graph (`"Improve team morale"`) reports "no obvious entry point" and
exits 1, rather than fabricating a plan. `goal decompose`/`goal suggest --apply`/
`goal plan --apply` all also accept `--capability <role>` on the task they
create — see [Swarm](#swarm), next.

### Swarm

Everyone sharing this workspace — humans and agents alike. An agent can
optionally declare a **capability** — `planner`, `backend`, `frontend`,
`database`, `testing`, `security`, `documentation` or `reviewer` — a stable
role a scheduler matches against, since the vocabulary of roles outlasts
whichever model happens to be filling one:

```bash
atlas --agent claude-1 swarm join claude-1 --kind claude --capability backend
atlas swarm list                 # online state, current task, and its goal
atlas swarm pause bob / resume bob / leave bob
```

```
$ atlas swarm list
● claude-1          claude     1 lease(s), task=T1, goal=G1, capability=backend
● cursor-1          cursor     0 lease(s)
```

When a task declares `--capability testing`, `continue` never hands it to an
agent without that capability — a hard gate, so idle backend agents don't
scoop up testing work just because nobody else has claimed it yet. It's a
gate on *automatic* claiming only: `assign` bypasses it entirely, since a
human deliberately handing work to someone is the override path, not a
scheduling decision.

### Continue

The loop verb an agent calls in a `while true`. Idle, it claims the highest-
priority task it can safely start and leases its scope in the same
transaction — no window where it's been handed work it doesn't yet own.
Already working, it renews what it holds instead of reaching for something
new:

```bash
atlas --agent claude-1 continue                 # anything startable
atlas --agent claude-1 continue --goal G1       # only work under one goal
```

### Assign

The human-steering counterpart to `continue`: hand a task (or a goal's next
startable task) to someone directly, instead of waiting for them to ask:

```bash
atlas assign T3 --to carol
atlas assign G1 --to carol             # picks G1's next startable task
atlas assign T3 --to carol --preempt --priority 9   # take it from a lower-priority holder
```

This moves the scope's leases too, atomically — the bug this fixed (a task
that was scoped but never claimed used to reassign with zero leases) is
covered by regression tests, since it's exactly the path the dashboard's
reassign button uses.

### Observe

The dashboard's five questions, as one command: what is everyone doing, what
is blocked and by whom, what is the critical path, and how much can run in
parallel:

```
$ atlas observe
who's doing what
  ● claude-1         claude     1 lease(s), task=T1, goal=G1
  idle: cursor-1

0 startable now · up to 0 in parallel · 0 in review
```

`--goal G1` restricts the view to one goal's subtree — useful once a
workspace has more than one thing in flight.

### Review

A checkpoint between "done editing" and "merged." Submitting keeps every
lease the task holds — nothing is available for anyone else to grab until a
human or another agent actually looks at it:

```bash
atlas --agent claude-1 review submit T1     # leases stay held
atlas --agent reviewer review approve T1    # releases leases, unblocks dependents
atlas --agent reviewer review reject T1 "missing a null check"  # reopens it, notifies the assignee
atlas review list                           # everything awaiting a look
```

By default the assignee can't approve their own submission — `--force`
overrides that for a human who wants to self-approve.

### Automatic notification on API changes

An agent's edit to a routed handler broadcasts to whoever's active work
depends on it, instead of requiring them to poll. If agent B is mid-task on
something that calls an endpoint agent A just changed, B's `request inbox`
gets an `api-change` notice — the sender is excluded, and the notice expires
on its own after an hour if nobody acts on it. This runs automatically
whenever `atlas scan <paths>` or `atlas index --watch` sees a modified,
routed (`api`-role) symbol; a full `atlas scan` with no paths (an initial
index build, almost always with no agents mid-task) skips the diff entirely.

When the changed symbol belongs to a task that belongs to a goal, the same
scan also opens a follow-up task for each impacted symbol that goal doesn't
already cover — a test calling the changed handler gets a `testing`-capability
task auto-opened under the same goal, for instance. Re-editing the same
symbol never opens a duplicate for something already covered. A changed
symbol outside any goal only notifies, exactly as above.

### Multiple repositories

A workspace can register more than one repository, so a goal like "add
refunds" can decompose into tasks spanning a frontend repo and a backend
repo without the swarm needing two separate coordination planes:

```bash
atlas repo add ../frontend --name web    # relative to the workspace root
atlas repo list
```

Every workspace has at least `R1`, registered automatically by `atlas init`
pointing at `.` — a single-repo workspace (the common case) is entirely
unaffected by any of this. `atlas index`/`atlas scan` with no path cover
every registered repo; a symbol reference gains an explicit `repo:path:Fqn`
form (`web:src/app.tsx:App`) for the rare case a path collides across two
repos, though a bare name still resolves as long as it's unambiguous.
Leases, tasks and goals never had to change to support this — they only
ever reference a symbol id, and identity already encodes which repo a
symbol belongs to. The one thing this does **not** do is resolve an import
across repos (a frontend package importing something published from the
backend repo) — that's a real, separate problem, not attempted here.

---

## Using it from an agent

Most agents should not do any of this — connect the tool over MCP
([above](#connect-your-coding-tool)) and the loop below runs itself. What
follows is the shell-level contract underneath it, for a CI worker, a script,
or a tool that does not speak MCP.

Every command takes `--json`, and the exit code is the answer:

| code | meaning |
| --- | --- |
| `0` | granted / clean / found |
| `1` | denied / violations / nothing ready — a normal answer, not an error |
| `2` | something went wrong (bad arguments, no workspace, unknown symbol) |

The everyday loop:

```bash
while atlas --json --agent "$AGENT" continue --goal "$GOAL"; do
    atlas --json --agent "$AGENT" context      # scope, callers, tests, decisions
    edit_the_code                              # ← the MCP adapter is what fills this in
    atlas check --agent "$AGENT" || fix_it_or_bail
    atlas --agent "$AGENT" review submit "$TASK"
done
```

Before an edit rather than after it — the version an editor hook runs on every
keystroke:

```bash
if ! atlas --json --agent "$AGENT" guard "$FILE"; then
    # Somebody else holds it. The report names them and says what to do.
    atlas --agent "$AGENT" request lease "$SYMBOL" --reason "$WHY" --wait 60
fi
```

The primitive version, for when a task needs a specific symbol rather than
whatever `continue` would pick:

```bash
if atlas --json --agent "$AGENT" lease acquire "$SYMBOL" --ttl 300 --wait 60; then
    edit_the_code
    atlas check --agent "$AGENT" || exit 1
    atlas lease release "$SYMBOL" --agent "$AGENT"
else
    # Blocked. Ask the holder for it instead of giving up or waiting blindly;
    # exit 0 means the symbol is now yours.
    if atlas --agent "$AGENT" request lease "$SYMBOL" --reason "$WHY" --wait 60; then
        atlas --agent "$AGENT" lease acquire "$SYMBOL"   # no-op if handed over
        edit_the_code
    else
        pick_another_task    # declined or expired, with a reason in the JSON
    fi
fi
```

The other half of the loop — answering, which is what makes the first half
work — is just as short:

```bash
# Drain the inbox and apply a policy. Nothing here needs a human.
atlas --json --agent "$AGENT" request inbox | jq -r '.[].id' | while read -r id; do
    if work_is_finished; then
        atlas --agent "$AGENT" request accept "$id"       # hands the lease over
    else
        atlas --agent "$AGENT" request decline "$id" --reason "busy, ~${eta}s"
    fi
done
```

Identity resolves from `--agent`, then `$ATLAS_AGENT`, then the name
registered in `.atlas/agent`.

Symbols can be referenced however is convenient — `processPayment`,
`PaymentService.processPayment`, `src/payments.ts:PaymentService.processPayment`,
`src/payments.ts` (the whole file), or the raw `s_…` id. Ambiguous names
produce an error listing the candidates rather than a guess.

---

## Command reference

### Everyday commands

| command | purpose |
| --- | --- |
| `atlas goal add <title> [--priority --description]` | state a goal |
| `atlas goal list` / `show <id>` | see goals, or one goal's progress and tasks |
| `atlas goal decompose <id> --task t [--symbol --dep --priority --capability]` | add an explicitly-scoped task under a goal |
| `atlas goal suggest <id> --near <symbol> [--depth --apply]` | propose tasks from the knowledge graph's impact radius |
| `atlas goal plan <id> [--depth --apply]` | propose tasks from the goal's own title/description, no `--near` needed |
| `atlas goal done\|abandon <id>` | close a goal |
| `atlas swarm join <name> [--kind --capability]` | join the workspace |
| `atlas swarm list` / `pause\|resume\|leave [name]` | who's here, and steer them |
| `atlas continue [--ttl --goal]` | keep going: renew what you hold, or claim the next thing |
| `atlas assign <task-or-goal-id> --to <agent> [--preempt --priority]` | hand work to someone directly |
| `atlas observe [--goal]` | what everyone's doing, what's blocked, what's parallel |
| `atlas review submit\|approve\|reject\|list <id>` | the approval checkpoint before work counts as done |
| `atlas check [paths] [--warn-only]` | enforce leases against the working tree |
| `atlas guard <path> [--symbol --strict]` | may I edit this *now*? exit 1 if someone else holds it |
| `atlas context [--task --agent --depth]` | scope, blast radius, tests, decisions — orientation in one packet |
| `atlas session list [--live]` / `end <id>` | which coding tools are attached |
| `atlas activity [--all --agent]` | who has their hands on which file right now |
| `atlas arch [--depth --repo --node]` | the repository as a person pictures it, and who is inside each box |
| `atlas hook install --claude-code --mcp` | wire an editor up so none of this is manual |
| `atlas status` | index, agents, leases, tasks, recent events |
| `atlas serve [--port]` | HTTP API, websocket stream, dashboard |

### Advanced / primitives

These are what the everyday commands are built on. They are not deprecated —
scripts written against them keep working exactly as they do today, and
reaching for one directly (a specific symbol, a specific lease) is sometimes
exactly what you want.

| command | purpose |
| --- | --- |
| `atlas init` | create `.atlas/runtime.db`, registering the default repo `R1` |
| `atlas repo add <path> [--name]` | register another repository under this workspace |
| `atlas repo list` | every registered repo, id/name/root |
| `atlas scan [paths] [--force]` | index the repo (incremental, .gitignore-aware) |
| `atlas index [--watch]` | index once, or stay current as files change |
| `atlas symbols [pattern] [--kind] [--role] [--path]` | list indexed symbols |
| `atlas api [pattern]` | every HTTP endpoint, with method and path |
| `atlas tests [symbol]` | which tests cover a symbol; exit 1 if none do |
| `atlas tables [name]` | database tables and the code that touches them |
| `atlas services` | services from manifests, and what they depend on |
| `atlas owners [path\|symbol]` | CODEOWNERS plus whoever holds a lease now |
| `atlas show <symbol>` | location, lease state, callers, callees, children |
| `atlas graph <symbol> [--depth]` | impact analysis with lease holders |
| `atlas agent register [--kind --capability]\|list\|leave\|heartbeat\|pause\|resume` | presence (`swarm` is the same identity, framed around the goal) |
| `atlas lease acquire <symbol> [--ttl --priority --task --wait --preempt]` | claim a symbol |
| `atlas lease release <symbol\|id> \| --all` | hand it back |
| `atlas lease renew` | heartbeat every held lease |
| `atlas lease transfer <symbol\|id> --to <agent>` | hand ownership over atomically |
| `atlas lease list [--mine]` / `queue <symbol>` / `check <symbol>` | inspect |
| `atlas diff [paths]` | symbol-level diff without enforcement |
| `atlas watch [--once --tail]` | follow the event bus |
| `atlas task add <title> [--symbol --dep --priority --capability]` | add a task, scoped to symbols |
| `atlas task scope <id> --symbol X` | declare what a task will touch |
| `atlas task next [--ttl]` | claim the best startable task, leasing its scope |
| `atlas task done <id> [--next] [--force]` | finish, release scope, optionally claim next |
| `atlas task assign <id> --to <agent>` | reassign a task, moving its leases (`assign` also resolves goal ids) |
| `atlas task priority <id> <n>` | reorder the queue |
| `atlas task list\|block\|fail` | the rest of the task graph |
| `atlas schedule [--infer]` | waves, contention, cycles, critical path |
| `atlas throughput [--minutes]` | completions, duration, denials, handovers |
| `atlas memory set\|get\|list\|rm` | shared project memory |
| `atlas msg send\|inbox\|read` | structured agent messages |
| `atlas request lease <symbol> [--reason --wait --deadline]` | ask the holder to hand it over |
| `atlas request interface <name> --to <agent> --method m` | ask for an interface you need |
| `atlas request depend --on-task T1` | declare a blocking dependency |
| `atlas request open --kind k --subject s [--to]` | any structured ask (omit `--to` to broadcast) |
| `atlas request inbox\|outbox\|list\|show` | see what is pending |
| `atlas request accept\|decline\|fulfill\|cancel` | answer |
| `atlas request wait <id> [--timeout]` | block until answered; exit 0 = fulfilled |
| `atlas progress [--percent --note --eta --task]` | report in (also heartbeats) |
| `atlas hook install\|uninstall [--git --claude-code --mcp]` | enforcement: git pre-commit, editor hooks, MCP registration (bare = git, as always) |
| `atlas mcp [--as --tool --heartbeat-secs --keep-leases]` | speak MCP on stdio; a client launches this, not you |

### HTTP API

`atlas serve` exposes the same runtime over HTTP, plus `/` serving a live
dashboard and `/ws` pushing two kinds of tagged frame:

```jsonc
{"type":"event","event":{…}}       // one line of the narrative, as it happens
{"type":"snapshot","status":…,"activity":…,"notifications":…,"arch":…,"plan":…}
```

A snapshot arrives first, so a client can paint from one frame without
fetching anything, and at most one is built per tick no matter how many events
landed in it. That coalescing is why the dashboard needs no polling loop.

```
GET    /api/status                  index, agents, leases, tasks, events
GET    /api/symbols?pattern=&kind=  search
GET    /api/graph/{symbol}?depth=   neighbours + impact
GET    /api/leases                  active leases
POST   /api/leases                  {symbol, agent, ttl_secs, priority, task}
                                    → 200 granted, 409 denied (with conflicts)
DELETE /api/leases/{id}?agent=      release
POST   /api/leases/{id}/renew?agent= heartbeat
POST   /api/leases/{id}/transfer    {from, to} — atomic handover
GET    /api/requests?agent=&direction=inbox|outbox|all
POST   /api/requests                {kind, from, to?, subject, symbol?, ...}
GET    /api/requests/{id}
POST   /api/requests/{id}/accept    {agent} — performs a lease handover
POST   /api/requests/{id}/decline   {agent, reason}
POST   /api/requests/{id}/fulfill   {agent, response}
GET    /api/progress                latest report per agent
POST   /api/progress                {agent, percent, note, eta_secs, symbol?}
GET    /api/endpoints               every HTTP endpoint in the repo
GET    /api/tables                  tables, with their accessors
GET    /api/services                services, with their dependencies
GET    /api/owners?path=            CODEOWNERS resolution
GET    /api/agents                  presence, including paused state and current goal
POST   /api/agents/{name}/pause     {paused}
GET    /api/goals                   every goal
POST   /api/goals                   {title, priority, description, created_by}
GET    /api/goals/{id}               one goal
GET    /api/goals/{id}/progress      task-state rollup + contributors
POST   /api/goals/{id}/decompose     {title, priority, deps, symbols} → a scoped task
GET    /api/goals/{id}/suggest?near=&depth=   impact-clustered task proposals
GET    /api/goals/{id}/plan?depth=  the same, seeded from the goal's own title/description
GET    /api/repos                   every registered repository
POST   /api/tasks/{id}/priority     {priority}
POST   /api/tasks/{id}/assign       {to, actor, preempt, priority} — settles remaining scope too
POST   /api/tasks/{id}/state        {state, agent, force}
POST   /api/tasks/{id}/review/submit    {agent}
POST   /api/tasks/{id}/review/approve   {agent, force}
POST   /api/tasks/{id}/review/reject    {agent, reason}
GET    /api/observe?goal=           agents + tasks + plan, fused for the "what's happening" view
POST   /api/continue                {agent, ttl, goal} → 200 task, 204 nothing startable
GET    /api/throughput?minutes=&buckets=
GET    /api/schedule?infer=         waves, contention, cycles, critical path
POST   /api/schedule/claim          {agent} -> 200 task, 204 nothing startable
GET    /api/events?since=&limit=    event log
GET    /api/check?agent=            enforcement report
POST   /api/scan                    {paths?, force?} re-index; with paths, notifies whoever is affected
GET    /api/guard?path=&agent=&symbol=   may this agent edit this, right now?
GET    /api/context?task=|agent=&depth=  orientation packet
GET    /api/arch?repo=&depth=       services, dependencies, tables + who is in each
GET    /api/arch/{node}?depth=      one box: workers, deps, goals, routes, impact
GET    /api/activity?agent=&all=    who is editing what, right now
GET    /api/notifications?agent=&limit=   what a human needs to be told
GET    /api/sessions?live=          which coding tools are attached
DELETE /api/sessions/{id}           disconnect one, handing its leases back
GET    /api/memory?tag=             shared project memory
POST   /api/memory                  {key, value, author?, tags?}
GET    /api/memory/{key}            one entry (404 if absent)
DELETE /api/memory/{key}
POST   /api/agents/{name}/capability {capability} — null makes it a generalist
DELETE /api/agents/{name}           evict, releasing its leases
POST   /api/tasks/{id}/scope        {symbols}
```

---

## Foundations — how the runtime actually works

This is implementation detail. None of it is something you should need for
day-to-day work — `goal`, `swarm`, `continue`, `assign`, `observe` and
`review` are built entirely out of the pieces below, and knowing they exist
matters mostly for extending the runtime, not using it.

### The repository knowledge graph

atlas does not re-read your repository on every prompt. It maintains a model
of it, and keeps that model current as files change.

**Nodes**

| node | where it comes from |
| --- | --- |
| service | a manifest — `Cargo.toml`, `package.json`, `go.mod`, `pyproject.toml`, `pom.xml` |
| file | any source file with a grammar |
| class, interface, function, method, type, constant | tree-sitter |
| table | `CREATE TABLE` / `ALTER TABLE` in a `.sql` file |

**Roles**, layered on top: a function that is routed is an `api` node; a
function that asserts is a `test`; a schema file is `schema`. An endpoint
keeps its method and path in metadata, so `GET /payments/{id}` is a thing you
can look up. `api`-role symbols are also what the automatic API-change
notification watches.

**Edges**

| edge | meaning |
| --- | --- |
| `contains` | service → file → class → method |
| `imports` | file imports file; rolled up, service depends on service |
| `calls` / `uses` | resolved *through imports*, not by name alone |
| `queries` | a function reads or writes a table |
| `tests` | a test exercises a symbol |

Ownership is modelled as an attribute of a path (from CODEOWNERS) rather than
as edges to person-nodes: people are not code, and making them nodes would
put them in the middle of every traversal.

```
$ atlas api
POST    /payments              createPayment  api/src/routes.ts:9
GET     /payments/:id          getPayment     api/src/routes.ts:16
DELETE  /payments/:id          voidPayment    api/src/routes.ts:20

$ atlas tables
payments         db/schema.sql:1   2 accessor(s)
    function  api/src/routes.ts:createPayment [POST /payments]
    function  api/src/routes.ts:getPayment    [GET /payments/:id]
audit_log        db/schema.sql:13  0 accessor(s)

$ atlas tests voidPayment
! no test reaches api/src/routes.ts:voidPayment      # exit 1 — wire it into CI
```

Try it: `bash demo/knowledge.sh`.

#### Imports are what make the graph true

Resolving a call by name alone fails the moment two files define `record()`.
So imports are extracted per language and resolved to repo files first, and a
call then resolves in the order a reader would use: something defined in this
file, then something this file imported, then a repo-wide unique name.
Anything still ambiguous is left unresolved rather than guessed at.

Third-party imports (`react`, `github.com/aws/aws-sdk-go`) resolve to
nothing, which is correct — they are real, but they are not nodes in *this*
repository.

#### Continuously updated

```bash
atlas index --watch      # or just `atlas serve`, which watches by default
```

A filesystem watcher re-indexes the files that changed, within a few hundred
milliseconds, and the graph reflects the edit — new route, new table
accessor, deleted symbol — with nobody running `atlas scan`. It's also what
triggers the automatic API-change notification: a targeted rescan diffs
before it re-indexes, so it can tell a routed symbol just changed.

### Symbols, not files

File locks are too coarse — two agents routinely need different functions in
the same file. atlas parses with tree-sitter and makes each **function,
method, class, interface, type, constant and module** a first-class object
with a stable id.

Ids are derived from *identity* (path + kind + qualified name), never from
content, so editing a function's body does not invalidate the lease you hold
on it. Renaming it does, which is correct: it is a different symbol now.

```
$ atlas symbols --kind method
method     src/payments.ts:PaymentService.processPayment  src/payments.ts:4
method     src/payments.ts:PaymentService.refund          src/payments.ts:9
```

### Leases, not locks

A lock held by a crashed process is a deadlock waiting for an operator. A
lease carries an owner, a task, a priority, a heartbeat and an expiry:

- **TTL** — the lease dies on its own if the agent stops heartbeating.
- **Heartbeat** — `atlas lease renew` pushes the expiry out while work
  continues.
- **Priority** — `--preempt` lets a strictly higher-priority request take
  over.
- **Queue** — denied requests take a numbered place in line, and the runtime
  will not hand the symbol to a later arrival ahead of them.

This is what `continue` and `assign` acquire on your behalf. A task's scope
and its leases are always granted and returned together — claiming, review
approval, reassignment and dead-agent reclamation all go through the same
`settle_scope_tx` primitive, so none of them can promise a scope the lease
layer then refuses.

### Containment-aware conflicts

Symbols nest, so leases must too. Holding `PaymentService` blocks its
methods; holding one method blocks the class; holding a file blocks
everything in it.

```
$ atlas --agent cursor-1 lease acquire SessionStore.create
✗ cannot lease src/auth.py:SessionStore.create
  ✗ inside src/auth.py:SessionStore, held by claude-1 (frees in ~4m59s)
```

### Enforcement

`atlas check` re-parses the working tree and diffs it against the index at
symbol granularity, then fails on anything the agent does not hold a lease
for.

It is precise about *which* symbol changed: editing a method changes the
class's text but not the class's own content, so the class is not reported.
Editing imports is a change to the file itself, and needs a lease on the
file.

```bash
atlas hook install     # wires `atlas check` into .git/hooks/pre-commit
```

| change | what covers it |
| --- | --- |
| edit a function body | a lease on that function, its class, or its file |
| add a method to a class | a lease on the enclosing class or file |
| delete a function | a lease on that function or its file |
| edit imports / top-level code | a lease on the file |
| create a brand new file | nothing — new files are always allowed |

### Negotiation

Avoiding each other is not enough. When one agent needs what another is
holding, it should be able to *ask* — and get a machine-readable answer.

```
$ atlas --agent hotfix request lease computeFee --reason "production hotfix" --wait 30
→ asked builder for src/payments.ts:computeFee (request req_c2df403419bb)

# builder's policy loop answers while it is still working:
✗ declined  hand over src/payments.ts:computeFee   busy at 40%, ~3s left

# ... and once it is finished:
✔ handed src/payments.ts:computeFee to hotfix
```

Requests are typed, carry a JSON payload, and expire:

| kind | meaning | how it resolves |
| --- | --- | --- |
| `lease-transfer` | "hand me that symbol" | accepting **performs the handover** |
| `interface` | "I need these methods to exist" | accept, then fulfil with a version |
| `dependency` | "I am blocked on task T3" | auto-fulfils when T3 completes |
| `review` | "your submission was sent back" | carries the rejection reason |
| `api-change` | "something you depend on just changed" | informational; self-expires |
| `question`, your own | anything else | accept / decline / fulfil |

Three properties make this work unattended:

- **Accepting a transfer is atomic.** The lease keeps its id and changes
  owner inside one transaction. Release-then-reacquire has a hole in the
  middle where a queued third agent can take the symbol; a transfer has no
  hole.
- **Requests self-resolve.** Release a symbol somebody asked for and their
  request is fulfilled with `symbol_free: true`. Finish (and have approved)
  a task somebody declared a dependency on and their request clears. The
  holder never has to remember who was waiting.
- **Deadlines expire**, exactly like leases. `atlas request wait` exits 1
  when the ask is declined or expires, so a waiting agent is never stuck.

Ownership can also move directly, without asking:

```bash
atlas lease transfer computeFee --to reviewer-agent
```

### Progress

Instead of other agents inferring state from silence:

```bash
atlas progress --percent 60 --note "authorize() done, capture() next" --eta 120
```

Progress doubles as a heartbeat — an agent reporting in is self-evidently
alive, so its leases are renewed by the same call. It shows up in `atlas
status`, `atlas observe`, on the dashboard, and on the event bus.

### The scheduler

`continue`, `assign` and the underlying `task next` all read a task graph
that carries a **scope** — the symbols each task will touch — and orders
work by more than declared priority:

```bash
atlas task add "refund flow"    --priority 9 --symbol voidPayment
atlas task add "ledger entries" --priority 3 --symbol record
atlas schedule --infer
```

```
inferred from the code graph
  + T1 depends on T2 (api/src/routes.ts:voidPayment calls)

wave 1 · can start now (2 of 3)
  ○ T2  ledger entries   pending  p3  ⟨lib/src/ledger.ts:record⟩
  ○ T3  read endpoint    pending  p5  ⟨api/src/routes.ts:getPayment⟩  ✗ held by human
wave 2 · after wave 1
  ◌ T1  refund flow      pending  p9 blocked by T2  ⟨api/src/routes.ts:voidPayment⟩
critical path: T2 → T1
```

Four things fall out of that one link to the graph:

- **Dependencies are inferred, not declared.** `voidPayment` calls `record`,
  so the task touching the caller waits for the task touching the callee —
  even though it has triple the priority. Nobody wrote that dependency down.
- **Work is never handed out to collide.** A ready task whose scope someone
  holds is skipped, with the holder named. Claiming a task leases its scope
  in the *same transaction*, so there is no window between being given work
  and owning it. Containment counts: holding a file blocks a task scoped to
  one function inside it.
- **The critical path is derived, not guessed.** Each wave is a causal
  chain — a task's dependency sits exactly one wave earlier — so walking
  backward from the deepest wave produces the sequence actually gating
  completion. (A task blocked on a dependency that's merely `running`, not
  yet done, can't appear on the chain until that dependency finishes — a
  read of the current state, not a limitation this feature adds.)
- **Dropped work comes back.** An agent that stops heartbeating has its
  task returned to the pool *and its scope released*, so the next agent can
  start it immediately instead of waiting out a TTL — capability-gated or
  not; reclaiming is unconditional, and only the next *claim* is gated.

```bash
atlas task next              # claim the best task you can safely start
atlas task done T2 --next    # finish, release the scope, take the next thing
```

Dependency cycles are reported rather than left to stall silently, and
finishing someone else's task is refused unless you pass `--force` — they
are probably still editing the symbols it reserved.

### The dashboard

`atlas serve` puts a live view of the workspace at `http://127.0.0.1:7373`.
It is the primary way a human understands what is happening, and it is
read-write.

The top row answers "what is going on": **Goals**, **Workers**, **Live
activity** and **Notifications**. Below it is the repository, and below that
whatever box you clicked.

| panel | what it tells you |
| --- | --- |
| **Live activity** | who has their hands on which file *right now*, with a percentage and an ETA — before any of it is committed |
| **Notifications** | contextual, not generic: *"createPayment changed — web depends on this, 2 follow-up tasks opened"*. Clicking one selects the box it happened in |
| **Repository** | services, the arrows between them, and the tables they touch. A box somebody is inside is outlined green with a pulsing dot; click it for who is in there, what it depends on, what depends on *it*, and what it exposes |
| **Workers** | filled dot = a coding tool is attached · hollow = a bare CLI loop heartbeating · grey = not seen recently |
| **Timeline** | the event bus, live |

and, underneath, the machinery: connected tools, the review queue, active
leases, what can execute next, the critical path, throughput, the task graph.

Read-write, as before:

| capability | what it does |
| --- | --- |
| **drag tasks** | drop one task onto another to reprioritise it |
| **pause agents** | stop the scheduler handing an agent new work; it keeps what it holds |
| **reassign work** | move a task to another agent — **its leases move with it**, atomically |
| **disconnect a tool** | press × on a session; its leases come straight back |
| **approve or reject reviews** | the review queue, straight from the dashboard |

Everything it shows is also a CLI command, because the dashboard is a thin
client over the same `Store` methods — including the picture:

```bash
atlas arch                       # the same graph, in a terminal
atlas arch --depth 2             # services, then their directories
atlas activity                   # who is editing what, right now
atlas agent pause bob            # and `agent resume`
atlas assign T3 --to carol       # moves the lease too
atlas throughput --minutes 30
```

**Nothing in the browser polls.** The page opens a websocket, and the daemon
pushes one coalesced state snapshot per tick — so a burst of two hundred
events is one repaint, not two hundred requests. The only timer left in the
page is the lease countdown, which needs no server at all.

Denials are the number worth watching. Rising denials mean the work is
carved up badly — two agents keep reaching for the same symbols — not that
the agents are slow.

### Shared workspace

Everything an agent would otherwise re-derive on every prompt:

- **presence** — who is connected, what they hold, what they are working on,
  and under which goal
- **task graph** — tasks with dependencies; `continue` hands out only
  unblocked work, highest priority first
- **shared memory** — architecture decisions, conventions, interfaces
- **messages** — structured agent-to-agent requests, not chat
- **event bus** — every acquire, denial, expiry, task transition, review
  decision, appended to a log that `atlas watch`, the dashboard and audits
  all read

### Impact analysis

The index is a graph, so an agent can ask what a change would disturb
*before* starting — including who currently owns the blast radius. This is
also what powers `goal suggest`'s task proposals and the automatic
API-change notification.

```
$ atlas graph computeFee --depth 3
impact of changing src/payments.ts:computeFee
  ·[1] src/payments.ts:PaymentService.processPayment  ← leased by claude-1 for 4m56s
```

---

## How it works

```
crates/
  atlas-core/     lang + parse   tree-sitter → symbols, references
                  imports        import extraction + module resolution
                  roles          endpoint and test detection
                  sql            tables from DDL, table refs in code
                  topology       services from manifests, CODEOWNERS
                  store          SQLite schema, event log, transactions
                  repo           registered repositories, repo-scoped resolution
                  scan           incremental indexer, prunes dead symbols
                  notify         diffs a targeted scan; broadcasts + opens
                                 follow-up tasks for goal-linked API changes
                  lease          acquire / renew / release / expire / queue
                                 / transfer
                  check          working-tree diff + enforcement
                  graph          callers, callees, impact
                  arch           the repository as a human pictures it:
                                 services, dependencies, tables, and who is
                                 inside each box right now
                  activity       who is editing which file this second
                  notice         what a human needs to be told, phrased for
                                 one — a query over requests + events
                  work           agents, task graph, memory, messages
                  goal           goals, decomposition, suggestion, progress
                  review         submit / approve / reject checkpoint
                  protocol       typed requests, negotiation, progress
                  schedule       waves, inference, conflict-free claiming,
                                 critical path
  atlas-daemon/   axum HTTP API, websocket event stream, dashboard
                  watcher        filesystem watcher, continuous re-indexing
  atlas-cli/      the `atlas` binary — goal/swarm/assign/observe/review/
                  continue plus every primitive underneath
```

**Coordination lives in the database, not in a server.** Every mutation runs
in a SQLite `IMMEDIATE` transaction, so two agents in two processes on two
machines sharing a checkout cannot both win the same symbol. Nothing has to
be running for `atlas lease acquire` — or `atlas continue` — to be correct;
the daemon adds observability, not safety. This is verified in the test
suite by racing eight real OS processes for one function and asserting
exactly one exits 0.

**Languages** — Rust, TypeScript, TSX, JavaScript, Python, Go, Java, C#,
C++, plus SQL for schemas. Each grammar's own `tags` query does symbol
extraction, so support tracks upstream tree-sitter rather than a
hand-maintained node table; imports use a small per-language query, and
roles use cross-framework heuristics rather than a rule per web framework.

### Testing

```bash
cargo test          # parser, ids, store, scan, imports, roles, sql, topology,
                    # leases, check, graph, scheduler, tasks, goals, review,
                    # repo, notify, memory, messages, negotiation, watcher —
                    # plus end-to-end tests driving the real binary across
                    # multiple processes
```

---

## Status

atlas is built as a stack, and it's worth being precise about which layer
each phase of the original plan belongs to:

```
Human Goal → Planning → Task Graph → Scheduler → Shared Memory
    → Knowledge Graph → Leases → Execution → Verification
```

**Built and working, top to bottom:** goals with both explicit and fully
automatic decomposition (`goal decompose`, `goal suggest`, `goal plan`);
presence and steering across the whole swarm, including capability-based
role matching (`swarm`, `assign`, `observe`); a review checkpoint before
work counts as done (`review`), with cascading follow-up tasks opened
automatically when a goal-linked API changes; priority-, dependency- and
capability-aware scheduling with critical-path analysis (`continue`,
`schedule`); multiple repositories under one workspace (`repo`); shared
memory, structured messages and an event bus; the repository knowledge
graph (services, files, symbols, tables, endpoints, tests, kept current by
a filesystem watcher) with automatic API-change notification; symbol
leasing with TTL, heartbeat, priority, queueing and preemption;
containment-aware conflict detection; enforcement; the negotiation protocol
with atomic ownership transfer; ownership from CODEOWNERS; the HTTP API and
the live dashboard, including goals, the review queue and repositories.

**The adapter layer** closes the one gap all of that left open: a coding tool
had no way to participate except by shelling out. `atlas mcp` is a single MCP
server that any MCP-speaking tool launches — Claude Code, Cursor, Codex,
Windsurf, Zed, Gemini CLI — and it registers, heartbeats, renews leases,
delivers notices and leaves cleanly without the model doing anything. Editor
hooks (`atlas hook install --claude-code`) make enforcement bite at the moment
of the edit rather than at commit time: an edit to a symbol another agent holds
is refused, with the holder named and a one-call path to negotiating for it.
`atlas guard` and `atlas context` expose the same two capabilities to anything
that can run a command.

Against the original 11-phase roadmap (`plan.md`), Phases 0 through 5 are
done. This pass substantially advanced two more: **Phase 6** (multi-repository)
now has its core — a workspace can register more than one repository, and a
goal can decompose into tasks spanning both, with identity, leasing and
scheduling entirely unaware repos exist — though the daemon/dashboard still
serve only the default repo, and cross-repo import resolution isn't
attempted (see Known limits). **Phase 10**'s "intelligent task decomposition"
gained both a graph-assisted mode (`goal suggest`) and a fully automatic one
seeded from the goal's own wording (`goal plan`) — still a keyword-matching
heuristic, not a generated plan, since there's no model in this runtime to
generate one. Phases 7-9 remain, and it's worth stating what each one means
in this vision's terms rather than as an isolated feature:

- **Phase 7, IDE plugins**, is now mostly answered by MCP rather than by one
  plugin per editor: every editor worth targeting already speaks it, so a
  human steering from inside their editor is just another participant in
  `swarm list`. What is left is native UI — showing lease state in the gutter
  — rather than integration.
- **Phase 8, distributed execution**, is more swarm members regardless of
  where they physically run — the coordination already lives in SQLite, not
  in a process, so a member running on a different machine already needs
  nothing new from the runtime.
- **Phase 9, persistent team memory**, is goal and review history becoming
  first-class recall — not just "what does this function do" but "why did
  we reject this approach in March, and who decided."

Picture the workspace a few years out: Alice runs Claude Code, Bob runs
Cursor, Charlie runs OpenCode, a CI agent runs the test suite on every
submission, and a docs agent keeps the README in sync — five participants,
two humans and three agents, all in one `swarm list`, all pulling from one
task graph, none of them able to silently collide because the lease layer
underneath refuses it structurally. Git stores that history. Kubernetes
schedules containers regardless of which machine they land on. The goal for
atlas is the equivalent for engineering work itself: schedule the work, not
just the workload, and let the person steering it think about goals instead
of processes.

Deliberately not built yet, in rough order of value:

- **Native editor UI** (the rest of Phase 7). MCP already connects every major
  tool, so what is missing is presentation — lease state in the gutter, a
  swarm panel — not integration.
- **atlas never calls a model.** No sampling, no orchestration, no agent loop:
  the runtime coordinates, the coding tool thinks. `atlas mcp` is a server, not
  a client, and it will not spawn an agent for you.
- **Remote MCP.** stdio only — no HTTP+SSE transport, no auth. Which is also
  why the daemon still assumes localhost.
- **Line-precise guarding.** `atlas guard` works at file granularity, or symbol
  granularity when the caller narrows to one. Mapping an edit's byte range back
  to a symbol is a real feature and a separate one.
- **Generative task decomposition.** `goal suggest`/`goal plan` cluster
  *where* an impact radius lands; neither proposes *what* the resulting
  tasks should actually do. That's the harder, model-backed half of Phase 10.
- **Type-aware resolution.** Calls resolve through imports, which handles
  most real code, but a method call on a value whose type comes from
  inference (`x.record()`) is resolved by name or not at all. That needs
  per-language type analysis.
- **Daemon/dashboard multi-repo routing** (the rest of Phase 6). The CLI's
  `scan`/`check`/`goal` commands already cover every registered repo; `atlas
  serve` still serves only the default one. Real, separate plumbing —
  deliberately not folded into this pass.
- **Cross-repo import resolution** (the rest of Phase 6). A frontend package
  importing something published from a registered backend repo doesn't
  auto-resolve — a genuinely harder, separate problem from registering the
  repos themselves.
- **Semantic leasing.** Leases are structural (symbols and scopes), not
  intent-based ("the authentication system").
- **CRDT co-editing inside a single symbol.** Leases are exclusive; two
  agents cannot safely share one function body. "Live editing" here means
  seeing *that* somebody is in a file, not seeing their keystrokes.
- **A rendered diff of uncommitted work.** The workspace shows who is editing
  what and how far along they say they are; it does not show the text of the
  change. Reading each other's buffers is a separate feature and a much
  larger one.
- **Negotiation policy.** The runtime carries asks and answers; deciding
  *when* to yield a lease is the agent's judgment, not the runtime's.
  `demo/negotiate.sh` shows one simple policy (decline while busy, accept
  when done).

### Known limits

- The index is a snapshot: `atlas scan` after pulling or after a large
  refactor. Enforcement compares the working tree against the last scan.
- A symbol that is renamed is a new symbol, and any lease on the old name is
  retired (with a `lease.dropped` event) on the next scan.
- Lease expiry is lazy — it happens when someone next writes, watches, or
  the daemon ticks. A `atlas lease list` may therefore be a beat behind
  wall-clock expiry, though acquisition itself never is.
- Files over 2 MB and languages without a configured grammar are skipped.
- Inferred dependencies are only as good as task scopes. A task with no
  `--symbol` still schedules, but contributes no inference and reserves
  nothing.
- An agent is presumed dead after 60s without a heartbeat, at which point
  its task is reassigned and its scope freed. Long-running agents must call
  `atlas lease renew`, `atlas progress`, or `atlas continue` — all three
  count as a heartbeat.
- The dashboard has no authentication and binds to localhost. It is an
  operator's window onto their own machine, not a service to expose.
- **Live activity is only as good as the tools reporting it.** atlas does not
  watch keystrokes and never reads an editor's unsaved buffer — it learns
  about an edit when a tool tells it, which happens in the pre-edit hook (one
  keystroke before the change lands) and again after. So a tool wired up with
  MCP or the editor hooks contributes file-granular, sub-second activity, and
  a tool wired up with neither contributes presence and nothing else. An edit
  window also closes on its own after 60s of silence, the same window as agent
  liveness, so a crashed editor stops claiming to be mid-edit.
- The repository picture is drawn from the index, so it is exactly as current
  as the last scan — which `atlas serve` keeps up to date by watching the
  filesystem. Rust `use` statements across workspace crates do not resolve to
  files (see type-aware resolution above), so a Cargo workspace shows its
  crates as boxes without the arrows between them; `atlas services` has always
  reported the same thing.
- Dragging a task writes a priority one step above or below its neighbour,
  so repeated drags drift the numbers. `atlas task priority` sets them
  exactly.
- Endpoint and test detection are heuristics, not framework plugins. They
  cover Express/Fastify/Hono, Flask/FastAPI, Spring, ASP.NET, chi/net-http
  and axum, and the common test conventions of each ecosystem; an unusual
  router will be missed rather than guessed at. Routes written in comments
  are skipped, but a route written in a *string fixture* inside a test will
  be indexed as real. `atlas api` is the way to check what was found — and
  what the automatic API-change notification is watching.
- Table references are matched against tables the repo actually declares,
  so a database defined outside the repository produces no `queries` edges.
- The watcher re-indexes changed files; deletions trigger a full rescan,
  because a removed manifest changes which service every file belongs to. A
  full rescan does not run the API-change diff (see Concepts, above).
- Requests expire lazily too, on the next write, watch or daemon tick — so a
  deadline can read as a second or two late, though `request wait` sweeps
  before every poll and never overshoots.
- A `lease-transfer` request is addressed to whoever held the symbol when it
  was opened. If that agent loses the lease in the meantime (preemption,
  expiry), accepting fails with "no longer holds" and the requester must ask
  again.
- Submitting a task for review only checks that the submitter is the current
  assignee; approving checks the opposite (anyone but the assignee, unless
  `--force`) — the two checks are intentionally asymmetric, not a shared
  guard, because "who may propose it's done" and "who may agree" are
  different questions.
- `atlas serve`/`atlas index --watch` follow the default repo (`R1`) only,
  even in a workspace with more than one registered — a documented scope
  limit for this pass, not a bug. `atlas scan`/`atlas index`/`atlas check`
  from the CLI already cover every registered repo.
- A symbol reference that doesn't specify a repo (a bare name, or a plain
  `path:Fqn`) searches across every registered repo; two repos sharing an
  identical relative path and fqn resolve ambiguously, exactly like two
  same-named symbols would within one repo — disambiguate with the explicit
  `repo:path:Fqn` form.
- Imports never resolve across repos, deliberately — a frontend repo
  importing a package published from a registered backend repo is treated
  the same as any other third-party import: real, but not a node in this
  workspace's graph.
