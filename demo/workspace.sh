#!/usr/bin/env bash
# The live collaborative workspace, end to end.
#
# Alice is on Cursor, Bob is on Claude Code. Both attach to the same golab
# workspace over MCP. Alice starts editing the payment API; Bob sees it before
# anything is committed. Alice changes a routed handler; Bob's agent is
# notified that his work depends on it. Nothing here runs `git`.
#
# Ctrl-C to stop; the workspace is deleted.
set -u

cd "$(dirname "$0")"
GOLAB="${GOLAB:-$(cd .. && pwd)/target/debug/golab}"
[ -x "$GOLAB" ] || GOLAB="$GOLAB.exe"
if [ ! -x "$GOLAB" ]; then
  echo "build it first:  cargo build" >&2
  exit 1
fi

PORT="${PORT:-7373}"
WORK="$(mktemp -d)"
TOOLS=""
cleanup() {
  [ -n "$TOOLS" ] && kill $TOOLS 2>/dev/null
  rm -rf "$WORK"
}
trap cleanup EXIT
cp -r knowledge/. "$WORK/"
cd "$WORK"

# The hooks hand golab native absolute paths; on MSYS `pwd` would give a POSIX
# one that no Windows binary can resolve back to the workspace.
ROOT="$(pwd -W 2>/dev/null || pwd)"

g() { "$GOLAB" "$@" > /dev/null 2>&1 || true; }
say() { printf '\n\033[1m%s\033[0m\n' "$1"; }
note() { printf '  %s\n' "$1"; }

say "1. a workspace, indexed"
g init

# `init` is the one step whose failure makes every later step meaningless, so
# it is the one step that gets checked. The usual cause is a shell and a binary
# that disagree about what a path is: WSL bash driving a Windows .exe hands it
# `/tmp/...`, which Windows reads as `C:\tmp\...` — a different directory that
# may not even exist. Everything then "runs" and nothing works.
if [ ! -f "$WORK/.golab/runtime.db" ]; then
  echo >&2
  echo "golab init did not create $WORK/.golab/runtime.db" >&2
  case "$GOLAB" in
    *.exe)
      if grep -qiE 'microsoft|wsl' /proc/version 2>/dev/null; then
        echo >&2
        echo "This looks like WSL running the Windows build. The two do not" >&2
        echo "share a filesystem namespace, so the .exe cannot see \$WORK." >&2
        echo >&2
        echo "Use one of:" >&2
        echo "  Git Bash:    bash demo/workspace.sh" >&2
        echo "  PowerShell:  powershell -File demo/workspace.ps1" >&2
        echo "  stay in WSL: cargo build   (inside WSL, for a Linux binary)" >&2
      fi
      ;;
  esac
  exit 1
fi
g index
note "$("$GOLAB" services | wc -l | tr -d ' ') services, $("$GOLAB" api | wc -l | tr -d ' ') endpoints"

say "2. a goal, broken into work"
# The scopes matter for what follows: bob's task covers a *test* that calls
# alice's handler, so when alice changes it the runtime can work out that bob
# is affected without anyone saying so.
g goal add "Add refunds to the payment API" --priority 9
g goal decompose G1 --task "refund on the create path" --symbol createPayment     --priority 9
g goal decompose G1 --task "cover it with tests"       --symbol testCreatePayment --priority 8
g task add "ledger entries" --priority 3 --symbol record
"$GOLAB" goal show G1 2>/dev/null | head -6

say "3. alice opens Cursor, bob opens Claude Code"
# Two long-lived MCP servers, driven the way a coding tool drives them. `tail
# -f` keeps each one's stdin open; a fresh process per message would be a fresh
# session, and EOF would hand every lease straight back.
for pair in "alice:cursor" "bob:claude-code"; do
  who="${pair%%:*}"; tool="${pair##*:}"
  : > "$who.mcp"
  tail -f -n +1 -s 0.1 --pid=$$ "$who.mcp" | "$GOLAB" mcp --as "$who" --tool "$tool" \
    > "$who.mcp.out" 2>/dev/null &
  TOOLS="$TOOLS $!"
  printf '{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"%s","version":"1.0"}}}\n' "$tool" >> "$who.mcp"
  printf '{"jsonrpc":"2.0","method":"notifications/initialized"}\n' >> "$who.mcp"
done
sleep 1
"$GOLAB" session list

say "4. work is handed out — and its scope is leased in the same breath"
call() {  # call <who> <tool> <json-args>
  printf '{"jsonrpc":"2.0","id":%s,"method":"tools/call","params":{"name":"%s","arguments":%s}}\n' \
    "$RANDOM" "$2" "$3" >> "$1.mcp"
}
# Assigned rather than claimed, so the story below is the same every run. Both
# paths lease the task's scope inside the same transaction as the assignment.
g assign T1 --to alice
g assign T2 --to bob
sleep 1
"$GOLAB" swarm list

say "5. alice starts editing — bob can see it before anything is committed"
# The pre-edit hook: this fires one keystroke *before* the change lands, which
# is the only moment at which "somebody is about to touch this" exists.
edit() {  # edit <agent> <event> <relative-path>
  printf '{"session_id":"s-%s","cwd":"%s","hook_event_name":"%s","tool_name":"Edit","tool_input":{"file_path":"%s/%s"}}' \
    "$1" "$ROOT" "$2" "$ROOT" "$3" |
    GOLAB_AGENT="$1" "$GOLAB" hook "$4" > /dev/null 2>&1
}
edit alice PreToolUse  api/src/routes.ts guard
call alice progress '{"percent":42,"note":"authorize() done, capture() next","eta_secs":300}'
sleep 1
"$GOLAB" activity

say "6. bob reaches for the same file, and is refused"
if edit bob PreToolUse api/src/routes.ts guard; then
  note "allowed (alice must have finished)"
else
  note "the edit was blocked — see the reason the model was given:"
  printf '{"session_id":"s-bob","cwd":"%s","hook_event_name":"PreToolUse","tool_name":"Edit","tool_input":{"file_path":"%s/api/src/routes.ts"}}' \
    "$ROOT" "$ROOT" | GOLAB_AGENT=bob "$GOLAB" hook guard 2>&1 >/dev/null | sed 's/^/    /'
fi

say "7. alice changes the endpoint's signature — bob is told, unprompted"
# `createPayment` is an api-role symbol, so a targeted rescan diffs it and
# broadcasts to whoever's *active work* sits in its impact radius. Bob's task
# covers the test that calls it, so bob is exactly that person. Nobody wrote
# down that these two pieces of work were related.
sed -i 's/export function createPayment(req) {/export function createPayment(req, idempotencyKey) {/' \
  api/src/routes.ts 2>/dev/null || true
edit alice PostToolUse api/src/routes.ts post-tool
g scan api/src/routes.ts
sleep 1
"$GOLAB" --agent bob request inbox 2>/dev/null | head -5

say "8. what a human sees"
"$GOLAB" arch

cat <<EOF

  dashboard:  http://127.0.0.1:$PORT

  what to look at, in order:
    · "Live activity"  — alice editing api/src/routes.ts, with a percentage
                         and an ETA, before any of it is committed
    · "Notifications"  — the api-change, saying which service depends on it
    · "Repository"     — the picture. Boxes somebody is inside are outlined
                         green with a pulsing dot; click one for who is in it,
                         what it depends on, and what it exposes
    · "Workers"        — filled dot = a coding tool is attached, hollow = a
                         bare CLI loop heartbeating

  things to try:
    · click "payments-api" on the picture, then a notification — it selects
      the box the change landed in
    · switch the picture to "+ directories", drag to pan, scroll to zoom
    · press × on alice in "Connected tools" — her leases come straight back
    · from another terminal, against the same workspace:
        cd $WORK
        $GOLAB activity                 # who is in which file, right now
        $GOLAB arch --depth 2           # the same picture, in a terminal

  no commits were needed for any of this to be visible.
  ctrl-c to stop (the workspace is deleted)

EOF

# Not `exec`: that would replace this shell and discard the EXIT trap, leaking
# the temp workspace every time the server is stopped.
"$GOLAB" serve --port "$PORT"
