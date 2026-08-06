#!/usr/bin/env bash
# The everyday vocabulary end to end: a human states a goal, agents decompose
# and continue it, one submits for review and another approves, and a third
# gets work handed to it directly. Runs in a throwaway copy.
set -u

cd "$(dirname "$0")"
GOLAB="${GOLAB:-$(cd .. && pwd)/target/debug/golab}"
[ -x "$GOLAB" ] || GOLAB="$GOLAB.exe"
if [ ! -x "$GOLAB" ]; then
  echo "build it first:  cargo build" >&2
  exit 1
fi

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
cp -r knowledge/. "$WORK/"
cd "$WORK"

say() { printf '\n\033[1m== %s\033[0m\n' "$1"; }
run() { printf '\033[2m$ golab %s\033[0m\n' "$*"; "$GOLAB" "$@"; }

"$GOLAB" init > /dev/null
"$GOLAB" index > /dev/null

say "1. a human states a goal — not a lease, not a task list"
run goal add "Support voiding a payment" --priority 9

say "2. two agents join the swarm"
run --agent claude-1 swarm join claude-1 --kind claude
run --agent cursor-1 swarm join cursor-1 --kind cursor

say "3. decompose it — explicitly, or let the graph suggest where the work lands"
run goal decompose G1 --task "wire the void endpoint" --priority 9 --symbol voidPayment
run goal suggest G1 --near voidPayment --depth 2
echo "   → advisory: names where the change touches, not what to do about it"

say "4. an agent asks to keep working — it gets handed the next startable task"
run --agent claude-1 continue --goal G1
echo "   → the task's scope is leased in the same transaction it was claimed in"

say "5. one view of the whole swarm: who's doing what, what's blocked, what's idle"
run observe --goal G1

say "6. claude-1 finishes and submits for review — leases are kept, nothing merges yet"
run --agent claude-1 review submit T1
run lease list
echo "   → still held: review is a checkpoint, not a handoff"

say "7. cursor-1 approves — leases release, and the goal's progress reflects it"
run --agent cursor-1 review approve T1
run goal show G1

say "8. a human reassigns the next task directly, no polling required"
run goal decompose G1 --task "ledger entry" --priority 5 --symbol record
run assign T2 --to cursor-1
run lease list

say "9. where the goal stands"
run goal show G1
