#!/usr/bin/env bash
# Walks through the whole runtime with two "agents" racing on one codebase.
# Runs in a throwaway copy of demo/src, so nothing here is mutated.
set -u

cd "$(dirname "$0")"
ATLAS="${ATLAS:-$(cd .. && pwd)/target/debug/atlas}"
[ -x "$ATLAS" ] || ATLAS="$ATLAS.exe"
if [ ! -x "$ATLAS" ]; then
  echo "build it first:  cargo build" >&2
  exit 1
fi

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
cp -r src "$WORK/src"
cd "$WORK"

say() { printf '\n\033[1m== %s\033[0m\n' "$1"; }
run() { printf '\033[2m$ atlas %s\033[0m\n' "$*"; "$ATLAS" "$@"; }

say "1. index the repository into a symbol graph"
run init
run scan
run symbols --kind method

say "2. two agents join the workspace"
run --agent claude-1 agent register claude-1 --kind claude
run --agent cursor-1 agent register cursor-1 --kind cursor

say "3. claude-1 leases a function; cursor-1 wants the same one"
run --agent claude-1 lease acquire PaymentService.processPayment --ttl 300 --task stripe
run --agent cursor-1 lease acquire PaymentService.processPayment --ttl 300 --task refunds
echo "   → denied, with the holder and the wait, not a merge conflict later"

say "4. disjoint work still runs in parallel"
run --agent cursor-1 lease acquire PaymentService.refund --ttl 300 --task refunds

say "5. leases nest: holding a class blocks its methods"
run --agent claude-1 lease check SessionStore.create
run --agent claude-1 lease acquire SessionStore --ttl 300
run --agent cursor-1 lease acquire SessionStore.create --ttl 300

say "6. enforcement: cursor-1 edits a function it does not hold"
sed -i.bak 's/const fee = computeFee(amount);/const fee = computeFee(amount) * 2;/' src/payments.ts
run --agent cursor-1 check
echo "   → exit $? (a pre-commit hook would stop here)"

say "7. the agent that holds the lease may make the very same edit"
run --agent claude-1 check

say "8. a crashed agent's lease expires on its own"
run --agent ghost lease acquire audit --ttl 2
echo "   (ghost dies without releasing; waiting for the TTL…)"
sleep 3
run --agent cursor-1 lease acquire audit --ttl 60
echo "   → no operator, no deadlock: the lease simply timed out"

say "9. what else a change would touch, and who owns it"
run graph computeFee --depth 3

say "10. the task graph hands out only unblocked work"
run task add "payment provider interface" --priority 5
run task add "refund flow" --priority 9 --dep T1
run --agent cursor-1 task next

say "11. shared memory instead of re-deriving context every prompt"
run --agent claude-1 memory set decision/fees "fees are basis points + 30c, computed in computeFee" --tag architecture
run memory list

say "12. the whole runtime at a glance"
run status --events 8

printf '\n\033[2mlive dashboard:  atlas serve   (http://127.0.0.1:7373)\033[0m\n'
