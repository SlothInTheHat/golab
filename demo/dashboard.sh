#!/usr/bin/env bash
# Seeds a workspace with a busy-looking swarm and serves the dashboard, so
# there is something to look at. Ctrl-C to stop; the workspace is deleted.
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

g() { "$GOLAB" "$@" > /dev/null 2>&1 || true; }

echo "seeding a workspace in $WORK …"
g init
# See demo/workspace.sh for why this one step is checked: a shell and a binary
# that disagree about what a path is (WSL bash driving a Windows .exe) make
# every later step fail in a way that looks like a golab bug.
if [ ! -f "$WORK/.golab/runtime.db" ]; then
  echo "golab init did not create $WORK/.golab/runtime.db" >&2
  if grep -qiE 'microsoft|wsl' /proc/version 2>/dev/null; then
    case "$GOLAB" in
      *.exe) echo "WSL cannot drive the Windows build — use Git Bash, or build inside WSL." >&2 ;;
    esac
  fi
  exit 1
fi
g index

for a in alice bob carol; do g --agent "$a" agent register "$a" --kind claude; done
g --agent dana agent register dana --kind cursor

# A backlog with real scopes, so the scheduler has something to reason about.
g task add "refund flow"            --priority 9 --symbol voidPayment
g task add "ledger: record entries" --priority 3 --symbol record
g task add "read endpoint"          --priority 5 --symbol getPayment
g task add "update the runbook"     --priority 1
g task add "audit trail"            --priority 7 --symbol auditPayment
g schedule --infer

# Put the swarm to work: two agents busy, one paused, one contended symbol.
g --agent alice task next
g --agent bob   task next
g --agent alice progress --percent 55 --note "ledger rows land, backfill next" --eta 240
g --agent bob   progress --percent 20 --note "reading the old handler"
g --agent dana  lease acquire getPayment --ttl 900
g --agent carol agent pause carol

# An open negotiation, so that panel is not empty either.
g --agent carol request lease getPayment --reason "needs the read path" --deadline 900

# Some finished work, so throughput has a duration to average.
g task add "spike: pagination" --priority 2
g --agent carol agent resume carol
CLAIMED=$("$GOLAB" --json --agent carol task next 2>/dev/null |
  python -c "import json,sys; print(json.load(sys.stdin).get('id',''))" 2>/dev/null || true)
sleep 1
[ -n "${CLAIMED:-}" ] && g --agent carol task done "$CLAIMED"
g --agent carol agent pause carol

# Two real coding tools attached over MCP, so "Connected tools" has something
# in it and the presence dots show all three states at once: alice and dana
# with a tool attached, bob and carol merely heartbeating from the CLI.
# `tail -f` keeps each server's stdin open the way an editor would.
for pair in "alice:claude-code" "dana:cursor"; do
  who="${pair%%:*}"; tool="${pair##*:}"
  : > "$who.mcp"
  tail -f -n +1 -s 0.1 --pid=$$ "$who.mcp" | "$GOLAB" mcp --as "$who" --tool "$tool" \
    > "$who.mcp.out" 2>/dev/null &
  TOOLS="$TOOLS $!"
  printf '{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"%s","version":"1.0"}}}\n' "$tool" >> "$who.mcp"
  printf '{"jsonrpc":"2.0","method":"notifications/initialized"}\n' >> "$who.mcp"
done
sleep 1

# And someone actually mid-edit, so "Live activity" and the green outlines on
# the repository picture have something in them. The pre-edit hook is the only
# thing that knows this moment exists.
ROOT="$(pwd -W 2>/dev/null || pwd)"
edit() {  # edit <agent> <event> <relative-path> <callback>
  printf '{"session_id":"s-%s","cwd":"%s","hook_event_name":"%s","tool_name":"Edit","tool_input":{"file_path":"%s/%s"}}' \
    "$1" "$ROOT" "$2" "$ROOT" "$3" |
    GOLAB_AGENT="$1" "$GOLAB" hook "$4" > /dev/null 2>&1
}
edit alice PreToolUse  api/src/routes.ts guard
edit bob   PostToolUse lib/src/ledger.ts post-tool
edit carol PreToolUse  api/src/routes.ts guard   # refused: alice is in there

cat <<EOF

  dashboard:  http://127.0.0.1:$PORT

  things to try in the browser:
    · drag a task in "Task graph" onto another to reprioritise it
    · use the dropdown on a task to reassign it (its lease moves too)
    · press "pause" on an agent — the scheduler stops giving it work
    · click a lease to inspect it
    · in "Connected tools", press × on alice — her leases come straight back,
      and her dot in "Agents" goes from filled (a tool is attached) to hollow
    · click a box in "Repository" — green outline means somebody is inside it
    · watch "Live activity", "Notifications" and the timeline while you poke

  and from another terminal, against the same workspace:
    cd $WORK
    $GOLAB --agent bob task done T3 --next
    $GOLAB --agent eve agent register eve
    $GOLAB --agent eve task next

  ctrl-c to stop (the workspace is deleted)

EOF

# Not `exec`: that would replace this shell and discard the EXIT trap, leaking
# the temp workspace every time the server is stopped.
"$GOLAB" serve --port "$PORT"
