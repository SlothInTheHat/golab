#!/usr/bin/env bash
# Tours the repository knowledge graph on a small two-service repo, and shows
# it staying current as a file changes. Runs in a throwaway copy.
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
cp -r knowledge/. "$WORK/"
cd "$WORK"

say() { printf '\n\033[1m== %s\033[0m\n' "$1"; }
run() { printf '\033[2m$ atlas %s\033[0m\n' "$*"; "$ATLAS" "$@"; }

say "1. index once: symbols, services, routes, tables, ownership"
run init
run index

say "2. services, discovered from manifests"
run services
echo "   → the dependency comes from a real import crossing the boundary"

say "3. the HTTP surface, without reading a router file"
run api

say "4. the database, and who touches it"
run tables
echo "   → audit_log has no accessors: dead schema, or a missing feature"

say "5. which tests cover a symbol — and which do not"
run tests createPayment
run tests voidPayment || echo "   → exit $?: an untested endpoint is a CI gate you can wire up"

say "6. blast radius of a change, across services"
run graph record --depth 3

say "7. ownership: CODEOWNERS plus whoever holds a lease right now"
run owners api/src/routes.ts
run --agent claude-1 agent register claude-1 --kind claude
run --agent claude-1 lease acquire createPayment --ttl 120
run owners createPayment

say "8. one lease on a service covers everything in it"
run --agent cursor-1 lease acquire service:payments-api || true

say "9. the graph stays current on its own"
"$ATLAS" index --watch > watch.log 2>&1 &
WATCHER=$!
sleep 1
printf '\033[2m# adding a route to api/src/routes.ts …\033[0m\n'
python - <<'PY'
import io
p = 'api/src/routes.ts'
s = io.open(p, encoding='utf-8').read()
s = s.replace(
    "  app.delete('/payments/:id', voidPayment);",
    "  app.delete('/payments/:id', voidPayment);\n  app.get('/payments/:id/audit', auditPayment);")
s += """
export function auditPayment(req) {
  return db.query("SELECT * FROM audit_log WHERE id = $1", [req.id]);
}
"""
io.open(p, 'w', encoding='utf-8', newline='\n').write(s)
PY
sleep 3
kill $WATCHER 2>/dev/null
wait $WATCHER 2>/dev/null
sed 's/^/   /' watch.log
echo "   (nobody ran atlas scan)"
run api
run tables audit_log

say "10. the whole repository, at a glance"
run status --events 0
