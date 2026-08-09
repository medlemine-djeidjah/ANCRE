#!/usr/bin/env bash
#
# The chaos pass (mvp-plan §5, M5).
#
#   ./deploy/compose/chaos.sh
#
# **This wipes the compose volumes and starts from an empty database.** That is
# not tidiness. `quickstart.sh` ends by deliberately corrupting a row, so a
# chaos run against whatever it left behind reports a broken chain that has
# nothing to do with the outage being tested — which is exactly what happened
# the first time these two were run in sequence. A gate that inherits another
# script's sabotage is a gate that cries wolf.
#
# Every claim this system makes about failure is a claim about a *dependency*
# being down, and until now all of them were tested against fakes. `bench`'s
# chaos gate proves the chaining and rollback logic survives an outage; it
# cannot prove that the ClickHouse client does, or that async-nats reports a
# failed publish the way the batcher expects. This script kills the real
# containers.
#
# Three outages, each with a claim that fails the script if it is false:
#
#   1. bus down     — the gateway keeps serving, evidence is lost *countably*,
#                     and the count arrives as an event when the bus is back
#   2. store down   — the gateway is unaffected, the ingester defers rather
#                     than acking, and the chain catches up with no gap
#   3. control down — the gateway serves from its installed snapshot until the
#                     staleness budget expires, then a High-risk system fails
#                     closed rather than serving unpinned traffic
#
# It is a gate, not a demo: every check below exits non-zero on a miss, and the
# third one fails if the gateway *keeps serving*, which is the direction a
# fail-closed test has to be written in.

set -euo pipefail

cd "$(dirname "$0")"

GATEWAY=${GATEWAY:-http://localhost:8080}
CONTROL=${CONTROL:-http://localhost:8081}
TENANT=acme
SYSTEM=hr-screening
DEMO_KEY=ancre-demo-key

# Matches compose. The gateway refuses High-risk traffic once its snapshot is
# older than this, so the third case has to outwait it.
STALENESS_BUDGET=${ANCRE_STALENESS_BUDGET_SECS:-30}

export ANCRE_OPENAI_BASE=http://mock-provider:9090

# Same demo cadence as the quickstart, so the pack this script verifies after
# the store outage is actually attested rather than merely self-consistent.
export ANCRE_CHECKPOINT_EVERY_N=20
export ANCRE_CHECKPOINT_EVERY_SECS=30
export ANCRE_CHECKPOINT_INTERVAL_SECS=5

ANCRE_BUILD_SHA=$(git -C ../.. rev-parse --short=12 HEAD 2>/dev/null || echo unknown)
export ANCRE_BUILD_SHA

say() { printf '\n\033[1m== %s\033[0m\n' "$*"; }
note() { printf '   %s\n' "$*"; }
die() { printf '\n\033[1;31m!! %s\033[0m\n' "$*" >&2; exit 1; }

compose() { docker compose --profile demo "$@"; }
verify_pack() { docker compose --profile tools run --rm verify --pack "$@"; }

# One request. Prints the status code and nothing else, so a caller can assert
# on it — the body is irrelevant to every claim here.
ask() {
  curl -sS -o /dev/null -w '%{http_code}' \
    "$GATEWAY/v1/chat/completions" \
    -H "Authorization: Bearer $DEMO_KEY" \
    -H 'Content-Type: application/json' \
    -d '{"model":"gpt-4o","messages":[{"role":"user","content":"Summarise this CV."}]}' \
    || echo 000
}

chain() { curl -sS "$CONTROL/v1/chains/$TENANT/$SYSTEM/events"; }
chain_length() { chain | grep -c . || true; }

# Count events of one type in the chain. Cheaper than a JSON parser and this
# script may not assume python is installed.
count_type() { chain | grep -c "\"event_type\":\"$1\"" || true; }

wait_until() {
  local label=$1 budget=$2 check=$3
  for i in $(seq 1 "$budget"); do
    printf '\r   %s (%ss)' "$label" "$i"
    if eval "$check"; then printf '\n'; return 0; fi
    sleep 1
  done
  printf '\n'
  return 1
}

say "Starting a clean stack"
note "This deletes the compose volumes: a chaos run has to start from a chain"
note "nobody has already tampered with."
docker compose --profile demo --profile tools down -v >/dev/null 2>&1 || true
compose up -d --build --wait >/dev/null \
  || die "compose could not bring the stack up. 'docker compose --profile demo logs' has the reason."

# A little traffic first, so there is a chain to break rather than an empty one.
for _ in $(seq 1 5); do
  [ "$(ask)" = 200 ] || die "the stack came up but will not serve. Nothing below would mean anything."
done
wait_until "waiting for the first events to chain" 60 '[ "$(chain_length)" -ge 5 ]' \
  || die "the ingester did not chain the warm-up traffic."

before=$(chain_length)
note "chain is $before events before we start breaking things"

# ---------------------------------------------------------------------------

say "1. The bus goes down"

compose stop nats >/dev/null
note "NATS stopped. The gateway has nowhere to put an audit event."

before_outage=$(chain_length)
sent=20
served=0
refused=0
for _ in $(seq 1 $sent); do
  code=$(ask)
  if [ "$code" = 200 ]; then served=$((served + 1)); else refused=$((refused + 1)); fi
done
note "$served served, $refused refused, with no bus at all"

# The whole design of the telemetry fork rests on this. A gateway that stops
# serving because it cannot record is a gateway nobody puts in a request path.
[ "$served" -eq "$sent" ] || die "the gateway refused $refused requests while the bus was down. Evidence must never be on the request path."

compose start nats >/dev/null
note "NATS back."

wait_until "waiting for the backlog to reach the chain" 90 \
  "[ \"\$(chain_length)\" -gt $before_outage ]" \
  || die "nothing reached the chain after the bus came back."
sleep 5

# The claim is **not** "an outage drops events" — the client buffers, and on a
# short outage nothing is lost at all, which is the better outcome and the one
# this run usually sees. The claim is that a gap is never *silent*: every
# request either produced an event in the chain, or is accounted for by a
# `telemetry.dropped` event covering the window it fell in.
#
# Asserting a drop instead of asserting the accounting is how a chaos test ends
# up demanding that a healthy system lose data. This script did exactly that on
# its first draft, and failed against a run in which nothing had gone wrong.
after=$(chain_length)
arrived=$((after - before_outage))
dropped_events=$(count_type telemetry.dropped)
lost=$((sent - arrived + dropped_events))

note "$sent requests, $arrived events chained, $dropped_events telemetry.dropped event(s)"

if [ "$arrived" -ge "$sent" ]; then
  note "nothing was lost: the client buffered through the outage and flushed on reconnect"
elif [ "$dropped_events" -ge 1 ]; then
  note "$lost event(s) lost, and the loss is itself in the chain — a countable gap"
else
  die "$((sent - arrived)) events never arrived and no telemetry.dropped accounts for them. A silent hole in the evidence is the one thing this must not do."
fi

# ---------------------------------------------------------------------------

say "2. The store goes down"

at_outage=$(chain_length)
compose stop clickhouse >/dev/null
note "ClickHouse stopped. The ingester has nowhere to put a chained event."

served=0
for _ in $(seq 1 20); do
  [ "$(ask)" = 200 ] && served=$((served + 1))
done
[ "$served" -eq 20 ] || die "the gateway was affected by a ClickHouse outage. It should not be able to tell."
note "$served served while the store was down"

compose start clickhouse >/dev/null
wait_until "waiting for the ingester to catch up" 120 \
  "[ \"\$(chain_length)\" -ge $((at_outage + 20)) ]" \
  || die "the ingester did not catch up. Events acked but not stored would be a gap the chain can never fill."

note "chain is now $(chain_length) events"

# The claim that matters: catching up must not have forked or gapped the chain.
mkdir -p evidence/chaos
chain > evidence/chaos/events.jsonl
curl -sS "$CONTROL/v1/checkpoints/$TENANT/$SYSTEM" -o evidence/chaos/checkpoints.json
curl -sS "$CONTROL/v1/pubkeys" -o evidence/chaos/pubkeys.json
cat > evidence/chaos/manifest.json <<JSON
{
  "pack_version": "ancre-pack/1",
  "tenant_id": "$TENANT",
  "system_id": "$SYSTEM",
  "created_at": "$(date -u +%Y-%m-%dT%H:%M:%SZ)",
  "produced_by": "ancre chaos pass"
}
JSON

verify_pack /evidence/chaos \
  || die "the chain does not verify after the outage. This is the failure the whole system exists to prevent."

# ---------------------------------------------------------------------------

say "3. The control plane goes down, past the staleness budget"

compose stop control >/dev/null
note "Control stopped. The gateway keeps its installed snapshot."

[ "$(ask)" = 200 ] \
  || die "the gateway refused immediately. A dead control plane is supposed to cost staleness, not availability."
note "still serving, inside the budget"

note "waiting out the ${STALENESS_BUDGET}s staleness budget…"
sleep $((STALENESS_BUDGET + 15))

code=$(ask)
# hr-screening is High-risk, and High-risk fails closed by default. Serving
# here would mean pinning a request to a configuration the node can no longer
# vouch for — which is exactly the pin an auditor would later find worthless.
[ "$code" = 503 ] \
  || die "the gateway answered $code on a High-risk system with a stale snapshot. It must fail closed: a pin nobody can vouch for is worse than a refused request."
note "HTTP $code — failed closed, as it must"

compose start control >/dev/null
wait_until "waiting for the gateway to recover" 60 '[ "$(ask)" = 200 ]' \
  || die "the gateway did not recover after the control plane came back."
note "serving again"

# ---------------------------------------------------------------------------

say "Chaos pass complete"
note "The bus, the store and the control plane were each killed under load."
note "The gateway never stopped serving except where it is designed to refuse,"
note "the chain came back with no gap and verified against a signature, and"
note "every request was accounted for — either as an event or as a counted loss."
