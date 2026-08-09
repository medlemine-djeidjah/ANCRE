# Deploying Ancre

For a platform engineer putting this in a real request path. If you only want
to see it work, run `./deploy/compose/quickstart.sh` and come back.

Three Ancre containers, three dependencies, and a ceiling on all of it: PRD §10
caps production at three Ancre services, because a mid-market platform team
will not adopt a fourth. Nothing below adds one.

---

## 1. What talks to what

```
       your application
              │  OpenAI-wire HTTP, base URL swapped
              ▼
        ┌───────────┐   pins resolved from an in-memory snapshot
        │  gateway  │   telemetry forked, never awaited
        └─────┬─────┘
              │ NATS JetStream (at-least-once)
              ▼
        ┌───────────┐   allocates seq, computes the chain
        │ ingester  │   ONE writer per (tenant, system)
        └─────┬─────┘
              │ batch insert
              ▼
         ClickHouse ◄──────────┐  reads ranges to seal
                               │
   Postgres ◄──┬──────► ┌──────┴────┐
   registry    │        │  control  │  signs checkpoints, publishes snapshots
   checkpoints │        └─────┬─────┘  serves the read API
   keys        └──────────────┘
                              │ NATS (config snapshots)
                              ▼
                           gateway
```

The gateway is the only thing on the request path. Everything else can be down
for a bounded period without a customer seeing an error — that boundary is the
staleness budget, and `deploy/compose/chaos.sh` is the test of it.

**One ingester per subject range.** Two ingesters consuming the same subjects
fork every chain they share, and a forked chain verifies perfectly on both
sides of the split — the worst kind of corruption, because nothing reports it.
Scale by giving each replica a disjoint `ANCRE_SUBJECT_FILTER`, never by adding
a replica to the same filter.

**One control plane, for now.** Two replicas can allocate generations in one
order and publish them in another (`docs/deferred.md`, D14). The gateway
installs whatever it is handed, so a fleet can flip between two configurations
until the next publish.

---

## 2. Configuration

Everything is an environment variable. There is no config file, on purpose: a
file is a thing that drifts between nodes and is not in anybody's deployment
manifest.

### Gateway

| Variable | Default | Notes |
|---|---|---|
| `ANCRE_LISTEN` | `0.0.0.0:8080` | |
| `ANCRE_CONTROL_URL` | `http://127.0.0.1:8081` | Poll backstop, and cold start |
| `ANCRE_CONTROL_TOKEN` | — | **Required** against a control plane with a token set. `/v1/snapshot` is protected and the gateway is a client of it |
| `ANCRE_NATS_URL` | `nats://127.0.0.1:4222` | Telemetry out, config in |
| `ANCRE_NODE_ID` | `$HOSTNAME`, else `gw-<pid>` | Goes into every event |
| `ANCRE_FAIL_CLOSED_ON_STALE` | `true` | `false` is a governance decision and is logged as one |
| `ANCRE_STALENESS_BUDGET_SECS` | `30` | How long this node may serve under a configuration it has not been able to confirm |
| `ANCRE_OPENAI_BASE` | `https://api.openai.com` | Point at Azure OpenAI or a self-hosted vLLM |
| `ANCRE_ANTHROPIC_BASE` | `https://api.anthropic.com` | |
| `ANCRE_OPENAI_API_KEY` | — | **The deployment's own key.** Never taken from a request |
| `ANCRE_ANTHROPIC_API_KEY` | — | |
| `RUST_LOG` | `info,ancre_gateway=debug` | |

An unset provider key is not an error — a self-hosted model behind an
overridden base URL needs none — but against a real provider it is a 401 on
every request. The gateway says which providers it has credentials for at
startup, and never prints the credentials themselves.

### Control plane

| Variable | Default | Notes |
|---|---|---|
| `ANCRE_LISTEN` | `0.0.0.0:8081` | |
| `DATABASE_URL` | **required** | `postgres://user:pass@host/db` |
| `CLICKHOUSE_URL` | `http://127.0.0.1:8123` | Read-only: sealing ranges and serving exports |
| `CLICKHOUSE_DB` / `CLICKHOUSE_USER` / `CLICKHOUSE_PASSWORD` | `ancre` / — / — | |
| `ANCRE_NATS_URL` | `nats://127.0.0.1:4222` | |
| `ANCRE_SIGNING_KEY_PATH` | `/var/lib/ancre/checkpoint-key` | Minted on first boot if absent. **Back it up** |
| `ANCRE_GATEWAY_VERSION` | this binary's `version+commit` | Only override during a deliberate mixed-build window |
| `ANCRE_CHECKPOINT_EVERY_N` | `10000` | Seal after this many new events on a chain |
| `ANCRE_CHECKPOINT_EVERY_SECS` | `300` | …or this long since its last checkpoint |
| `ANCRE_CHECKPOINT_INTERVAL_SECS` | `30` | How often the checkpointer is offered a turn |
| `ANCRE_ADMIN_TOKEN` | generated per process | The dashboard's login and the read API's credential. Unset means one is minted at boot and printed once — fine for a demo, wrong for anything you sign in to twice |

### Ingester

| Variable | Default | Notes |
|---|---|---|
| `ANCRE_NATS_URL` | `nats://127.0.0.1:4222` | |
| `ANCRE_SUBJECT_FILTER` | `ancre.events.>` | **The sharding key.** See the one-writer rule above |
| `ANCRE_CONSUMER_NAME` | `ancre-ingester` | Give each shard its own durable name |
| `CLICKHOUSE_URL` / `CLICKHOUSE_DB` / `CLICKHOUSE_USER` / `CLICKHOUSE_PASSWORD` | as above | Read-write |
| `ANCRE_NODE_ID` | `$HOSTNAME`, else `ing-<pid>` | Recorded on every event it chains |

The subject is `ancre.events.<tenant>.<system>`, so a shard is a subject
pattern: `ancre.events.acme.>` gives one ingester every chain belonging to one
tenant.

---

## 3. Schema and first boot

Both databases are created from the files in `deploy/compose/init/`, which the
official Postgres and ClickHouse images run automatically on an empty data
directory. Outside compose, apply them yourself:

```sh
psql "$DATABASE_URL" -f deploy/compose/init/postgres/001_registry.sql
clickhouse-client --queries-file deploy/compose/init/clickhouse/001_audit_events.sql
```

`audit_events` is **frozen** (mvp-plan §4). Adding a hashed column changes the
canonical encoding, which means every chain written before the change verifies
under a different rule set. `canon_version` exists so that if you must break
it, you break it explicitly.

Start order does not matter — every Ancre service exits loudly when a
dependency is unreachable at boot and is expected to be restarted — but the
control plane should reach a healthy Postgres before a gateway asks it for a
snapshot, or the gateway spends its 60-second cold-start budget waiting.

**A gateway will not bind its socket until it has a snapshot.** A closed port
reads as "not ready" to a load balancer; a port that answers 503 reads as a
node worth sending traffic to.

---

## 4. Onboarding a system

There is no write API in the MVP: the registry is edited out of band and the
control plane notices within ten seconds. That is four inserts.

```sql
BEGIN;

-- 1. The prompt, addressed by its own content hash.
--    Compute it with the workspace primitive, never with sha256sum:
--      cargo run -p ancre-gateway --example key-hash -- "$(cat prompt.txt)"
INSERT INTO prompts (prompt_hash, body)
VALUES (decode('<64 hex>', 'hex'), convert_to('<the prompt text>', 'UTF8'));

-- 2. The system. `risk_class` decides fail-closed behaviour, so it is a
--    governance decision, not a label.
INSERT INTO systems (system_id, system_version, ifu_version, risk_class,
                     policy_id, policy_version, default_route)
VALUES ('cv-screening', '1.0.0', 'ifu-2026-01', 'high', 'none', 'none', 0);

-- 3. Routes, in first-match-wins order. `default_route` indexes this list by
--    position, so the catch-all belongs last and `default_route` names it.
INSERT INTO routes (system_id, position, matcher, model_id, model_version,
                    prompt_id, prompt_version, prompt_hash)
VALUES ('cv-screening', 0, '"Any"'::jsonb,
        'gpt-4o', 'gpt-4o-2024-08-06',
        'cv-screen', 'b3:<first 12 hex of the prompt hash>',
        decode('<64 hex>', 'hex'));

-- 4. A virtual key. Mint it however you mint secrets; store only its hash:
--      cargo run -p ancre-gateway --example key-hash -- <the key>
INSERT INTO api_keys (key_hash, tenant_id, system_id)
VALUES (decode('<64 hex>', 'hex'), 'acme', 'cv-screening');

COMMIT;
```

`model_version` is what the registry **expects**. What gets recorded is what
the provider **answers with** — if those differ, or the provider returns a
floating alias, the event carries `unresolved:<alias>` and the
`unpinned_model` risk flag. That disagreement is the finding, so do not
"fix" it by loosening the registry.

Matchers are a small recursive enum, stored as jsonb:

```json
"Any"
{"Path": "/v1/chat/completions"}
{"ModelAlias": "claude-sonnet-4-5"}
{"Header": {"name": "x-team", "value": "risk"}}
{"All": [{"ModelAlias": "gpt-4o"}, {"Header": {"name": "x-team", "value": "risk"}}]}
```

A shape this build does not recognise is **refused**, not defaulted to `Any` —
a route that silently matched everything would repoint traffic without changing
a pin anyone can see.

Revoke a key by setting `revoked_at`; the next publish drops it from the
snapshot. Do not delete the row — a revoked key still explains historical
events.

---

## 5. Access control

The read API is split, and the split is the security model:

| Open, no credential | Protected by `ANCRE_ADMIN_TOKEN` |
|---|---|
| `GET /healthz` | `GET /v1/chains` and `/v1/chains/…/events`, `/summary` |
| `GET /v1/checkpoints/{tenant}/{system}` | `GET /v1/snapshot` |
| `GET /v1/pubkeys` | `GET /v1/prompts/{hash}` |

The left column is what an auditor needs to *check* evidence they were already
handed. A checkpoint is a signature over a root hash and a public key is a
public key: neither reveals a customer's traffic, and an auditor who has to
obtain a credential before verifying a signature is an auditor who verifies
less. The right column is the customer's business — no prompts or completions,
but `system_id`, timings, token counts and model versions are a competitive
picture of how they run their AI.

Two ways to present the token:

```sh
# machines
curl -H "Authorization: Bearer $ANCRE_ADMIN_TOKEN" localhost:8081/v1/chains

# browsers — sets an HttpOnly, SameSite=Strict session cookie for 8 hours
curl -X POST localhost:8081/api/session -H 'content-type: application/json' \
  -d "{\"token\":\"$ANCRE_ADMIN_TOKEN\"}"
```

**The gateway is a client too.** It reads `/v1/snapshot`, so it needs
`ANCRE_CONTROL_TOKEN` set to the same value, or it will refuse to bind its
socket at cold start — which is the correct failure, and the log line names the
variable.

What this is not: user accounts. One shared secret, no roles, no per-tenant
scoping, and no record of who read what. Anyone who can log in can read every
tenant's chains. That is tracked as D22 and it is the first thing to fix before
two customers share a deployment.

**The compose file ships a default token** (`ancre-insecure-default`) so that
`docker compose up` needs no configuration. It is a published string, the
control plane warns about it on every boot, and any deployment reachable by
anyone you do not trust must set a real one in `deploy/compose/.env`.

## 6. The dashboard

Served by the control plane itself at its own port — static assets compiled
into the binary, so there is no fourth container and no second origin. Sign in
with the operator token.

It shows chains, their counted shape, and every event's full pin set. What it
deliberately does **not** show is a green "verified" tick: the server rendering
that page is the server that stores the events, so any claim it makes about
their integrity is unverifiable by construction. It reports which ranges carry
a signature, and hands over an evidence pack to check somewhere else.

## 7. Key custody

The checkpoint signing key is the root of the whole evidence claim. In the MVP
it is a control-plane-local file, minted on first boot if absent and written
back to the path it was asked for, so a restart signs with the same key and the
checkpoints it already produced keep verifying.

- **Back up `ANCRE_SIGNING_KEY_PATH`.** Losing it does not invalidate existing
  checkpoints — the public halves stay exported with their windows — but no new
  checkpoint can continue the same key's history.
- Mount it read-only from a secret store *after* first boot. It cannot be
  read-only on first boot, because the control plane has to write it.
- Rotation is: stop the control plane, install a new key file, start it. Every
  key that was ever active stays in `signing_keys` with its window, and the
  windows abut exactly — an auditor verifying a two-year-old range gets the key
  that signed it, not the current one.
- Enterprise moves this to Vault. Say so before the security questionnaire
  asks, because it will.

The public keys are served unauthenticated at `GET /v1/pubkeys`. **Give the
fingerprint to an auditor out of band**, over a channel that is not the same
one the evidence arrives on. `ancre-verify --key <hex>` pins it, and then a
forged pack fails; without it, verification proves internal consistency and
says nothing about provenance. The verifier prints that distinction itself.

---

## 8. Health, and what to alert on

| Check | Endpoint |
|---|---|
| Control plane ready | `GET /healthz` |
| Gateway ready | any request; an unauthenticated one answers 401 once the socket is bound |

Worth an alert:

- **Checkpoint lag.** `SealReport::max_lag` is the largest unsealed run across
  all chains, logged every tick. It is the window in which tampering would go
  unattested, and it is the number an auditor will ask about.
- **`telemetry.dropped` events appearing at all.** Each one is a hole in a
  customer's evidence, and it names the chain and the window.
- **503s from the gateway.** With `ANCRE_FAIL_CLOSED_ON_STALE=true` these mean
  a High-risk system is being refused because the node cannot confirm its
  configuration — an availability symptom whose cause is in the control plane.
- **The startup line `this node's build and the snapshot's gateway_version
  disagree`.** Expected during a rolling deploy, a defect if it persists: the
  `gateway_version` pin will name the control plane's build and not the code
  that actually served the request.

---

## 9. What is not production-ready

Read `docs/deferred.md` in full before running this in front of real traffic.
The entries that most often change a deployment decision:

- **Auth is one shared token** (D22). It protects the content endpoints, which
  is the gap that mattered — but there are no identities, no per-tenant
  scoping, and no access log. Anyone who can sign in reads every tenant.
- **Dedupe is a bounded in-memory window** of 100 000 `event_id`s (D10). A
  redelivery older than that doubles an event, and the checkpointer then
  refuses that chain's range for good.
- **Heartbeats only cover chains the ingester has already seen** (D15), so a
  system that goes quiet across a restart stops emitting the daily heartbeat
  that makes its silence countable.
- **CI has never run** (D5). Every number in this repository was measured on
  one developer's machine.
