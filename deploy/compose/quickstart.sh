#!/usr/bin/env bash
#
# The quickstart. One command, no API key, ten minutes.
#
#   ./deploy/compose/quickstart.sh
#
# What it does, in order:
#
#   1. builds and starts six containers, seeded with one tenant and one system
#   2. sends traffic through the gateway — plain and streamed, one routed to
#      Anthropic instead of OpenAI, one with an overridden pin, and one the
#      provider answers with a floating alias
#   3. waits for the ingester to chain those events and the control plane to
#      sign a checkpoint over them
#   4. exports an evidence pack and verifies it offline, in a container with no
#      network interface
#   5. edits one row directly in ClickHouse and verifies again, which fails
#
# Step 5 is the point of the whole product. Everything before it is setup.
#
# It is deliberately noisy: a quickstart that prints nothing but a checkmark is
# a quickstart nobody believes.

set -euo pipefail

cd "$(dirname "$0")"
REPO_ROOT="$(cd ../.. && pwd)"

GATEWAY=${GATEWAY:-http://localhost:8080}
CONTROL=${CONTROL:-http://localhost:8081}
TENANT=acme
SYSTEM=hr-screening
# In the repository, and that is what makes it a demo key: it binds to one
# seeded tenant on a deployment whose provider is a mock.
DEMO_KEY=ancre-demo-key
PACK_DIR="evidence/${TENANT}-${SYSTEM}"

# Every pin the gateway writes will name this build. Passed as a build arg
# because the image does not contain `.git` — see deploy/compose/Dockerfile.
#
# The `.dirty` suffix is the same one `build.rs` applies in a checkout, and it
# is applied here too because setting the variable bypasses that check: a pin
# naming a commit whose code is not the code that ran would be a lie, and this
# script is the place most likely to produce one.
ANCRE_BUILD_SHA=$(git -C "$REPO_ROOT" rev-parse --short=12 HEAD 2>/dev/null || echo unknown)
if [ "$ANCRE_BUILD_SHA" != unknown ] &&
   [ -n "$(git -C "$REPO_ROOT" status --porcelain --untracked-files=no)" ]; then
  ANCRE_BUILD_SHA="$ANCRE_BUILD_SHA.dirty"
fi
export ANCRE_BUILD_SHA

# The demo's provider, so nobody needs an OpenAI account to watch a chain get
# built. `mock-provider` is only reachable inside the compose network.
export ANCRE_OPENAI_BASE=http://mock-provider:9090
export ANCRE_ANTHROPIC_BASE=http://mock-provider:9090

# A checkpoint every 20 events or 30 seconds, rather than the shipped default
# of 10 000 or 5 minutes. This is the one setting the quickstart changes purely
# for the sake of the demo, and it is a real configuration knob rather than a
# special case in the code: a chain nothing has signed yet is a chain an
# auditor cannot rely on, and waiting five minutes to show that would put the
# most important half of this demo past most people's patience.
export ANCRE_CHECKPOINT_EVERY_N=20
export ANCRE_CHECKPOINT_EVERY_SECS=30
export ANCRE_CHECKPOINT_INTERVAL_SECS=5

say() { printf '\n\033[1m== %s\033[0m\n' "$*"; }
note() { printf '   %s\n' "$*"; }
die() { printf '\n\033[1;31m!! %s\033[0m\n' "$*" >&2; exit 1; }

# The stack, and the verifier. Two profiles rather than one: `verify` is a
# one-shot tool, and putting it in the same profile as the services would mean
# `up --wait` waiting forever for a container whose job is to exit.
compose() { docker compose --profile demo "$@"; }
verify_pack() { docker compose --profile tools run --rm verify --pack "$@"; }

# ---------------------------------------------------------------------------

say "Building and starting six containers"
note "First run compiles the workspace in release mode — several minutes."
note "Build: $ANCRE_BUILD_SHA"
compose up -d --build --wait || die "compose could not bring the stack up. 'docker compose --profile demo logs' has the reason."

say "Sending traffic through the gateway"

ask() {
  local label=$1 body=$2
  shift 2
  note "$label"
  curl -sS -N -o /dev/null -w '     HTTP %{http_code} in %{time_total}s\n' \
    "$GATEWAY/v1/chat/completions" \
    -H "Authorization: Bearer $DEMO_KEY" \
    -H 'Content-Type: application/json' \
    "$@" \
    -d "$body"
}

for _ in 1 2 3 4 5; do
  ask "a screening request" \
    '{"model":"gpt-4o","messages":[{"role":"user","content":"Summarise this CV."}]}'
done

ask "a streamed one — tokens forwarded as they arrive" \
  '{"model":"gpt-4o","stream":true,"messages":[{"role":"user","content":"Summarise this CV."}]}'

# Same client, same OpenAI-wire request, a different provider. The gateway
# translates the body, rewrites the path to Anthropic's `/v1/messages`, and
# reads the pin back out of a response document with entirely different field
# names. The caller changed one string.
ask "the same request, routed to Anthropic instead" \
  '{"model":"claude-sonnet-4-5","messages":[{"role":"user","content":"Summarise this CV."}]}'

# The provider will answer with the floating alias it was asked for, because
# the mock only resolves ids it actually knows. That is the honest case and it
# is in the demo on purpose: the event records `unresolved:gpt-4o-preview` and
# raises the `unpinned_model` risk flag, which is a finding an auditor acts on.
ask "one the provider will not pin — expect a risk flag" \
  '{"model":"gpt-4o-preview","messages":[{"role":"user","content":"Summarise this CV."}]}'

# A caller pinning their own version over the registry's. A governance event:
# the resolver flags it and the flag rides on the event that used it.
ask "one where the caller overrode a pin" \
  '{"model":"gpt-4o","messages":[{"role":"user","content":"Summarise this CV."}]}' \
  -H 'x-ancre-pin-prompt-version: b3:0000deadbeef'

say "Waiting for the ingester to chain them, and the control plane to sign"

head_seq() {
  curl -sS "$CONTROL/v1/chains/$TENANT/$SYSTEM/events" 2>/dev/null | grep -c . || true
}
signed_count() {
  curl -sS "$CONTROL/v1/checkpoints/$TENANT/$SYSTEM" 2>/dev/null \
    | grep -o '"seq_to"' | grep -c . || true
}

for i in $(seq 1 60); do
  events=$(head_seq); events=${events:-0}
  signed=$(signed_count); signed=${signed:-0}
  printf '\r   %s events chained, %s checkpoint(s) signed (%ss)' "$events" "$signed" "$i"
  [ "$events" -ge 9 ] && [ "$signed" -ge 1 ] && break
  sleep 1
done
printf '\n'
events=$(head_seq); [ "${events:-0}" -ge 9 ] || die "the chain is short: the ingester is not keeping up. 'docker compose logs ingester'."

say "Building an evidence pack"

mkdir -p "$PACK_DIR"
curl -sS "$CONTROL/v1/chains/$TENANT/$SYSTEM/events"   -o "$PACK_DIR/events.jsonl"
curl -sS "$CONTROL/v1/checkpoints/$TENANT/$SYSTEM"     -o "$PACK_DIR/checkpoints.json"
curl -sS "$CONTROL/v1/pubkeys"                         -o "$PACK_DIR/pubkeys.json"

# The manifest is descriptive and unsigned, and the verifier says so. What
# actually binds the pack together is the checkpoints: ed25519 signatures over
# tree roots that the verifier recomputes from the events themselves.
cat > "$PACK_DIR/manifest.json" <<JSON
{
  "pack_version": "ancre-pack/1",
  "tenant_id": "$TENANT",
  "system_id": "$SYSTEM",
  "created_at": "$(date -u +%Y-%m-%dT%H:%M:%SZ)",
  "produced_by": "ancre quickstart, build $ANCRE_BUILD_SHA",
  "source": "$CONTROL"
}
JSON

note "$(wc -l < "$PACK_DIR/events.jsonl") events, $(du -sh "$PACK_DIR" | cut -f1) in $PACK_DIR"

say "Verifying it offline — in a container with no network interface"

# `network_mode: none` on the verify service. The claim is that verification
# needs no network; a container that has none is the only version of that claim
# a sceptic cannot argue with.
verify_pack "/evidence/${TENANT}-${SYSTEM}" \
  || die "the pack did not verify, which on a fresh run is a bug in Ancre and not in your machine."

say "Now tampering with the database the evidence lives in"

note "Editing one row directly in ClickHouse, with full DDL rights:"
note "  ALTER TABLE ancre.audit_events UPDATE tokens_out = 999 WHERE seq = 3"
compose exec -T clickhouse clickhouse-client --user ancre --password ancre \
  --query "ALTER TABLE ancre.audit_events UPDATE tokens_out = 999 WHERE seq = 3 AND system_id = '$SYSTEM' SETTINGS mutations_sync = 2"

curl -sS "$CONTROL/v1/chains/$TENANT/$SYSTEM/events" -o "$PACK_DIR/events.jsonl"

set +e
verify_pack "/evidence/${TENANT}-${SYSTEM}"
code=$?
set -e

if [ "$code" -eq 1 ]; then
  say "That is the product"
  note "Exit code 1. The chain named the altered event by its sequence number,"
  note "and the signature over the range it sits in no longer matches the events."
  note "Append-only is enforced by the hash chain, not by the database — the"
  note "store is not trusted, and neither is whoever runs it."
else
  die "tampering was not detected (exit $code). That is a serious bug: please open an issue."
fi

say "Where to go next"
note "The chain, as newline-delimited JSON:"
note "  curl -s $CONTROL/v1/chains/$TENANT/$SYSTEM/events"
note "The prompt every event pins, served by its own content hash:"
note "  curl -s $CONTROL/v1/prompts/6a4913393d5480619887cbb83ed4d49296cdeb23b94aacfb4618fbb5597fd7a6"
note "Stop everything, keeping the data:      docker compose --profile demo down"
note "Stop everything and delete the data:    docker compose --profile demo down -v"
