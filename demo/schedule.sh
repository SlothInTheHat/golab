#!/usr/bin/env bash
# The scheduler: work ordered by the code graph, handed out without collisions,
# and taken back from agents that die. Runs in a throwaway copy.
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
kill_heartbeat() {
  python - "$1" <<'PY'
import sqlite3, sys
c = sqlite3.connect('.golab/runtime.db')
c.execute("UPDATE agents SET heartbeat_at = 0 WHERE name = ?", (sys.argv[1],))
c.commit()
PY
}

"$GOLAB" init > /dev/null
"$GOLAB" index > /dev/null
for a in alice bob carol; do
  "$GOLAB" --agent "$a" agent register "$a" --kind claude > /dev/null
done

say "1. a backlog, each task scoped to the symbols it will touch"
run task add "refund flow"            --priority 9 --symbol voidPayment
run task add "ledger: record entries" --priority 3 --symbol record
run task add "read endpoint"          --priority 5 --symbol getPayment
run task add "update the runbook"     --priority 1
echo "   → not one dependency was declared"

say "2. the scheduler reads the call graph and orders the work itself"
run schedule --infer
echo "   → T1 has the highest priority but sits in wave 2:"
echo "     voidPayment calls record, so it has to wait for T2"

say "3. meanwhile a human is already editing getPayment"
run --agent human lease acquire getPayment --ttl 300

say "4. two agents pull work, and neither is handed the contended task"
run --agent alice task next
run --agent bob   task next
echo "   → T3 outranks T2 but is held, so alice was given T2 instead"
run lease list
echo "   → claiming leased each task's scope in the same transaction"

say "5. a third agent finds nothing it can safely start"
run --agent carol task next || echo "   → exit $?: it explains itself instead of colliding"

say "6. alice finishes, and takes the next thing in the same command"
run --agent alice task done T2 --next
echo "   → finishing released T2's scope, which unblocked T1"

say "7. closing someone else's work is refused"
run --agent bob task done T1 || echo "   → exit $?: bob is not holding it, and alice still is"

say "8. alice dies mid-task; the work comes back on its own"
run --agent alice progress --percent 40 --note "halfway through the refund flow"
printf '\033[2m# alice stops heartbeating …\033[0m\n'
kill_heartbeat alice
run schedule
echo "   → T1 is back in wave 1, with no operator involved"
run --agent carol task next
echo "   → carol picked up exactly where alice dropped it"

say "9. where the plan stands"
run status --events 6
