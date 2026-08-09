# Putting Ancre in your pipeline

Adoption is a base URL and a key. Nothing else changes — not the SDK, not the
request shape, not the response shape. That is the whole design constraint on
the ingress: the moment integration needs a code change, the platform engineer
who has to approve it starts asking what else is being bolted onto their
inference path.

---

## 1. Point your application at it

The gateway speaks the OpenAI wire. Every official SDK works unmodified.

### Python

```python
from openai import OpenAI

client = OpenAI(
    base_url="http://ancre-gateway.internal:8080/v1",
    api_key="<your Ancre virtual key>",   # not your OpenAI key
)

resp = client.chat.completions.create(
    model="gpt-4o",
    messages=[{"role": "user", "content": "Summarise this CV."}],
)
```

### TypeScript

```ts
import OpenAI from "openai";

const client = new OpenAI({
  baseURL: "http://ancre-gateway.internal:8080/v1",
  apiKey: process.env.ANCRE_KEY,
});

const resp = await client.chat.completions.create({
  model: "gpt-4o",
  messages: [{ role: "user", content: "Summarise this CV." }],
});
```

### curl

```sh
curl -s http://localhost:8080/v1/chat/completions \
  -H "Authorization: Bearer $ANCRE_KEY" \
  -H 'Content-Type: application/json' \
  -d '{"model":"gpt-4o","messages":[{"role":"user","content":"Summarise this CV."}]}'
```

Streaming works the same way and is a straight passthrough: the first token is
forwarded before the second is read, and observation happens on bytes already
in flight.

### The key is not your provider key

The `api_key` your application sends is a **virtual key** that names a system
in Ancre's registry. Its hash is what the registry stores, so a registry dump
is not a list of working credentials. The real provider credential lives in the
gateway's environment and is never taken from a request — see `docs/deploy.md`
§2 and §4.

### Switching provider is a registry row

```python
client.chat.completions.create(model="claude-sonnet-4-5", messages=[...])
```

Same client, same OpenAI-wire request. The gateway translates the body,
rewrites the path to Anthropic's `/v1/messages`, authenticates with the
deployment's Anthropic key, and reads the pin back out of a response document
with entirely different field names. Your application changed one string.

What it will **not** do is translate what it cannot translate faithfully. Tool
calls, `response_format`, and multiple system messages have no exact Anthropic
equivalent, so a request using them against an Anthropic route is refused with
a 400 that names the field. A wrong audit event is worse than a rejected
request.

---

## 2. What you get back that you did not have before

Nothing in the response — the response is the provider's, byte for byte. What
changes is that a verifiable record now exists:

```sh
curl -s localhost:8081/v1/chains/acme/hr-screening/events | head -1 | jq '.emitted.pins'
```

```json
{
  "config_generation": 1,
  "config_hash": "…",
  "system_id": "hr-screening",
  "system_version": "2.4.1",
  "ifu_version": "ifu-2026-03",
  "model_id": "gpt-4o",
  "model_version": "gpt-4o-2024-08-06",
  "prompt_id": "cv-screen",
  "prompt_version": "b3:6a4913393d54",
  "policy_id": "none",
  "policy_version": "none",
  "gateway_version": "0.1.0+a77de9bbc5cc",
  "risk_class": "high",
  "resolved_stale": false,
  "risk_flags": []
}
```

Every one of those is resolved **once**, at admission, and carried immutably
for the life of the request. A ninety-second streaming completion reports the
configuration it started under, not the one that landed mid-stream.

Two fields deserve attention in a review:

- `model_version` comes from the provider's response, never from your request.
  `unresolved:gpt-4o` with the `unpinned_model` flag means the provider would
  not say which weights ran. That is a finding, not a bug.
- `resolved_stale` and the `stale_config` flag mean the node was serving under
  a configuration it could not confirm. On a High-risk system it will refuse
  instead, so you will see 503s rather than flagged events.

---

## 3. Overriding a pin from the caller

```sh
curl ... -H 'x-ancre-pin-prompt-version: b3:9f2c1a'
```

Precedence is request header > virtual key binding > route rule > system
default. Any override raises `pin_overridden` on the event that used it, which
is exactly what an auditor wants to find. Use it for a canary, not as a
configuration channel — the registry is the configuration channel, and a fleet
configured by request headers has no snapshot anybody can reconstruct.

---

## 4. Running it in CI

Two useful shapes, and they answer different questions.

### a. Prove the evidence path still works

The gateway is a container and the verifier is a container. A CI job that
starts the stack, drives your own application's integration tests through it,
and verifies the resulting chain tells you that *your* traffic produces
evidence that verifies — not merely that ours does.

```yaml
- name: Bring up Ancre
  run: |
    export ANCRE_OPENAI_BASE=http://mock-provider:9090   # or your real key
    docker compose -f deploy/compose/compose.yaml --profile demo up -d --wait

- name: Run your integration tests against the gateway
  env:
    OPENAI_BASE_URL: http://localhost:8080/v1
    OPENAI_API_KEY: ancre-demo-key
  run: pytest tests/integration

- name: The chain those tests produced must verify
  run: |
    mkdir -p evidence/ci
    curl -sS localhost:8081/v1/chains/acme/hr-screening/events > evidence/ci/events.jsonl
    curl -sS localhost:8081/v1/checkpoints/acme/hr-screening > evidence/ci/checkpoints.json
    curl -sS localhost:8081/v1/pubkeys                        > evidence/ci/pubkeys.json
    printf '{"pack_version":"ancre-pack/1","tenant_id":"acme","system_id":"hr-screening"}' \
      > evidence/ci/manifest.json
    docker compose -f deploy/compose/compose.yaml --profile tools \
      run --rm verify --pack /evidence/ci
```

The verifier exits 0 clean, 1 violations, 2 cannot verify. Failing the build on
1 is the point; failing it on 2 as well is usually right, because "this build
cannot check that rule set" in your own CI means a version skew you want to
know about.

### b. Catch a substantial modification before it ships

A configuration change that may constitute a substantial modification is
recorded as a `config.generation.applied` event carrying the
`substantial_candidate` risk flag and the before/after `config_hash`. A CI job
that applies your registry change to a staging control plane and then queries
for that flag turns a legal review trigger into a pull-request comment:

```sh
curl -s "localhost:8081/v1/chains/$TENANT/$SYSTEM/events" \
  | jq -r 'select(.emitted.event_type == "config.generation.applied")
           | select(.emitted.pins.risk_flags[]? == "substantial_candidate")
           | "generation \(.emitted.pins.config_generation): review required"'
```

**It flags; a human decides; the decision is logged.** No code anywhere may
treat that flag as a determination — under the Act a substantial modification
can reset a grandfathering position, and that is a legal call, not a diff.

### The two gates in this repository

```sh
./deploy/compose/quickstart.sh   # packaging works, tampering is caught
./deploy/compose/chaos.sh        # the three outages behave as documented
```

Both exit non-zero on a miss and both are safe to run in CI, though `chaos.sh`
wipes its volumes and spends minutes waiting out a staleness budget. The
quickstart is wired into `.github/workflows/ci.yml`; the chaos pass is not, and
that is logged as debt (D20).

---

## 5. Failure modes your application will actually see

| Status | Meaning | What to do |
|---|---|---|
| `401` | No bearer token, or a key the registry does not know | Check the key hash is in `api_keys` and not revoked |
| `400` | The request cannot be faithfully translated for the routed provider | Route that system to OpenAI, or stop using the untranslatable field |
| `503` | Cold start, or stale configuration on a High-risk system | The gateway cannot pin this request. Look at the control plane, not the gateway |
| provider's own status | Passed straight through | It is the provider's answer, recorded as such |

A `503` from a fail-closed decision is the one worth designing for. It is
deliberate: a High-risk request the gateway cannot pin is a request whose
audit event would be worthless, and refusing is the honest outcome. Retry it —
the gateway recovers within one poll interval of the control plane returning.

**A request refused at admission produces no audit event.** It never reached a
model, so there is no decision to record, and a fabricated event with `unknown`
pins would be worse than none. Your own 4xx/5xx rate is the signal there.
