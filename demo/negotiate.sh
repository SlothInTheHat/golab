#!/usr/bin/env bash
# Two autonomous agent loops resolving a conflict between themselves.
#
# `builder` is mid-task and holds computeFee(). `hotfix` needs the same symbol
# for a production fix. Nobody types anything: builder runs a policy loop that
# declines while it is busy and accepts once it is done, and hotfix retries
# until it is granted, then edits and verifies.
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
cp -r src "$WORK/src"
cd "$WORK"

g() { "$GOLAB" "$@"; }
say() { printf '\033[2m%s\033[0m\n' "  $*"; }

g init > /dev/null
g scan > /dev/null
g --agent builder agent register builder --kind claude > /dev/null
g --agent hotfix  agent register hotfix  --kind cursor > /dev/null

# Open requests addressed to $1, one id per line. No colour when piped, so the
# plain output is parseable without extra tooling.
inbox_ids() { g --agent "$1" request inbox | awk '/^\? /{print $2}'; }

printf '\n\033[1m== builder starts work and takes the symbol\033[0m\n'
g --agent builder lease acquire computeFee --ttl 180 --task fees
g --agent builder progress --percent 20 --note "rewriting the fee table" --eta 6

# ---------------------------------------------------------------- builder loop
# Policy: while there is still work to do, decline and say how long. Once the
# work is finished, hand the symbol over to whoever asked.
builder_loop() {
  for pct in 40 70 100; do
    sleep 2
    g --agent builder progress --percent "$pct" --note "fee table $pct%" --eta $(( (100 - pct) / 20 )) > /dev/null
    for id in $(inbox_ids builder); do
      if [ "$pct" -lt 100 ]; then
        g --agent builder request decline "$id" --reason "busy at ${pct}%, ~$(( (100 - pct) / 20 ))s left"
      else
        g --agent builder request accept "$id"
      fi
    done
  done
}

# ----------------------------------------------------------------- hotfix loop
# Policy: try to take it; if it is held, ask for it and keep asking until the
# holder agrees or the attempts run out.
hotfix_loop() {
  sleep 1
  printf '\n\033[1m== hotfix needs the same symbol\033[0m\n'
  if g --agent hotfix lease acquire computeFee --ttl 180 --no-queue > /dev/null 2>&1; then
    say "it was free after all"
  else
    g --agent hotfix lease check computeFee
    for attempt in 1 2 3 4 5; do
      printf '\n\033[1m== hotfix asks (attempt %s)\033[0m\n' "$attempt"
      if g --agent hotfix request lease computeFee \
            --reason "production hotfix" --priority 9 --deadline 12 --wait 12; then
        break
      fi
      sleep 1
    done
  fi

  # A request can also be fulfilled by the holder simply releasing, in which
  # case nobody handed us anything — make sure we actually hold it.
  g --agent hotfix lease acquire computeFee --ttl 180 --task hotfix > /dev/null

  printf '\n\033[1m== hotfix edits the symbol it now owns\033[0m\n'
  sed -i.bak 's/return Math.round(amount \* 0.029) + 30;/return Math.round(amount * 0.019) + 25;/' src/payments.ts
  g --agent hotfix check
  g --agent hotfix lease release --all > /dev/null
}

builder_loop &
BUILDER=$!
hotfix_loop
wait $BUILDER 2>/dev/null

printf '\n\033[1m== the negotiation, as recorded on the event bus\033[0m\n'
g watch --once --since 0 | grep -E 'lease|request|progress'

printf '\n\033[1m== final state\033[0m\n'
g status --events 0
