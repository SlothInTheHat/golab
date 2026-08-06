#!/usr/bin/env bash
# Two coding tools sharing one repository through the MCP adapter.
#
# `alice` is on Claude Code, `bob` is on Cursor. Neither runs a lease command;
# both are driven entirely through the MCP tool surface, the way a real editor
# drives them. Both processes stay alive for the whole demo, because that is
# what an open editor is — and it is what lets the runtime heartbeat them,
# renew their leases and hand back their work when they disconnect.
#
# What it shows is the part that used to need a human: bob is refused an edit
# alice owns, asks for it in one call, and is handed it the moment she accepts.
#
# Runs in a throwaway copy.
set -u

cd "$(dirname "$0")"
ATLAS="${ATLAS:-$(cd .. && pwd)/target/debug/atlas}"
[ -x "$ATLAS" ] || ATLAS="$ATLAS.exe"
if [ ! -x "$ATLAS" ]; then
  echo "build it first:  cargo build" >&2
  exit 1
fi

WORK="$(mktemp -d)"
PIDS=""
cleanup() {
  # Closing the pipe is what an editor quitting looks like: the server sees
  # EOF, ends its session and hands its leases back. The `tail` feeding it
  # exits on its own via --pid, so there is nothing here to wait on.
  [ -n "$PIDS" ] && kill $PIDS 2>/dev/null
  sleep 0.3
  rm -rf "$WORK" 2>/dev/null
}
trap cleanup EXIT
cp -r src "$WORK/src"
cd "$WORK"

g() { "$ATLAS" "$@"; }
say() { printf '\033[2m%s\033[0m\n' "  $*"; }
step() { printf '\n\033[1m== %s\033[0m\n' "$*"; }

g init > /dev/null
g index > /dev/null
g goal add "Cut the payment fee" --priority 9 > /dev/null
g goal decompose G1 --task "rework the fee table" --priority 9 --symbol computeFee > /dev/null

# --------------------------------------------------------------- the adapters
#
# Each tool gets a `atlas mcp` that stays attached for the whole demo, the way
# it would under a real client — that is what lets the runtime heartbeat it and
# reclaim its work when it goes. Frames are appended to a file that `tail -f`
# streams into the server's stdin, which keeps stdin open between steps.
# Its stdout is a protocol stream, so it goes to a file we read frames out of.
start() { # name tool
  : > "$1.cmds"
  # `-s 0.1` because tail polls once a second by default, which would make
  # every reply land a step late. `--pid=$$` so the feeder dies with this
  # script rather than outliving it.
  tail -f -n +1 -s 0.1 --pid=$$ "$1.cmds" | g mcp --as "$1" --tool "$2" > "$1.out" 2>/dev/null &
  PIDS="$PIDS $!"
  # `initialize` is the whole registration story: by the time the first tool
  # call lands, the agent exists, holds a session and is being heartbeated.
  printf '{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"%s","version":"1.0"}}}\n' "$2" >> "$1.cmds"
  printf '{"jsonrpc":"2.0","method":"notifications/initialized"}\n' >> "$1.cmds"
}

ID=0
alice_seen=0
bob_seen=0

call() { # name tool-name args-json
  ID=$((ID + 1))
  printf '{"jsonrpc":"2.0","id":%d,"method":"tools/call","params":{"name":"%s","arguments":%s}}\n' \
    "$ID" "$2" "$3" >> "$1.cmds"
}

# Print the text half of whatever arrived since last time. Every result carries
# both a compact summary and the full structured payload; the summary is what a
# model actually reads, so it is what this prints.
drain() { # name
  sleep 0.6
  local file="$1.out" seen_var="$1_seen" seen total
  eval "seen=\$$seen_var"
  total=$(wc -l < "$file" 2>/dev/null || echo 0)
  if [ "$total" -gt "$seen" ]; then
    tail -n +$((seen + 1)) "$file" \
      | grep -o '"text":"[^"]*"' \
      | sed -e 's/^"text":"//' -e 's/"$//' -e 's/\\n/\n/g' \
      | sed 's/^/  /'
  fi
  eval "$seen_var=$total"
}

start alice claude-code
start bob cursor
sleep 1.2

step "alice opens Claude Code, bob opens Cursor"
say "neither runs an Atlas command — the handshake registered them both"
g session list

step "alice is handed work"
call alice next_task '{}'
call alice task_context '{}'
drain alice
say "→ she holds her task's scope, having asked for neither the task nor the lease"

step "bob goes to edit the same file"
say "the guard is asked before the edit, not after the commit"
call bob check_edit '{"path":"src/payments.ts"}'
drain bob

step "so bob asks for it — one call, and he never names who to ask"
call bob ask '{"kind":"lease-transfer","symbol":"computeFee","reason":"production hotfix"}'
drain bob
REQ="$(g --json request list | grep -o '"id": *"[^"]*"' | head -1 | sed 's/.*"\([^"]*\)"$/\1/')"
say "→ the runtime knew alice held it and addressed $REQ to her"

step "alice hears about it on her next tool call, without asking"
say "MCP has no way to push at a model, so notices ride along on every result"
call alice whoami '{}'
drain alice

step "she accepts; the symbol changes hands in that same transaction"
call alice respond "{\"request\":\"$REQ\",\"action\":\"accept\"}"
drain alice
g lease list

step "and now bob may edit it"
call bob check_edit '{"path":"src/payments.ts"}'
call bob progress '{"percent":80,"note":"lowering the fee"}'
drain bob

step "what a human sees"
g observe
g session list
g watch --once --since 0 | grep -E 'session|lease|request' | tail -8

step "the same workspace, live"
say "atlas serve   → connected tools, who holds what, the critical path"
